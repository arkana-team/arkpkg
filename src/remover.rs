use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use crate::database::Database;
use crate::errors::{ArkError, Result};
use crate::logger::Logger;
use crate::prompt::PromptHandler;

pub struct Remover<'a> {
    db: &'a Database,
    logger: &'a Logger,
    prompt: PromptHandler,
}

impl<'a> Remover<'a> {
    pub fn new(db: &'a Database, logger: &'a Logger, prompt: PromptHandler) -> Self {
        Self { db, logger, prompt }
    }

    /// Executes the 10-step package removal sequence.
    pub fn remove(&mut self, pkg_name: &str) -> Result<()> {
        self.logger.info(&format!("Removing package {}", pkg_name));

        // 1. Read the package's .arkinfo
        let arkinfo = match self.db.read_arkinfo(pkg_name) {
            Ok(info) => info,
            Err(_) => {
                return Err(ArkError::PackageNotFound(format!(
                    "Package '{}' is not installed",
                    pkg_name
                )));
            }
        };

        // Refuse an unconfirmed removal that would leave installed dependents broken.
        let installed_packages = self.db.read_installed_packages()?;
        let mut dependents = Vec::new();
        for (other_name, _) in &installed_packages {
            if other_name.eq_ignore_ascii_case(pkg_name) {
                continue;
            }
            let other_info = self.db.read_arkinfo(other_name)?;
            let depends_on_package = other_info.dependencies.iter().any(|dependency| {
                let direct_match = dependency.name.eq_ignore_ascii_case(pkg_name);
                let provided_match = arkinfo
                    .provides
                    .iter()
                    .any(|provide| provide.name.eq_ignore_ascii_case(&dependency.name));
                (direct_match || provided_match) && dependency.is_satisfied_by(&arkinfo.version)
            });
            if depends_on_package {
                dependents.push(other_name.clone());
            }
        }
        if !dependents.is_empty()
            && !self.prompt.ask(&format!(
                "Package '{}' is required by: {}. Remove anyway?",
                pkg_name,
                dependents.join(", ")
            ))?
        {
            return Err(ArkError::UserCancelled);
        }

        let preserved_files = resolve_preserved_files(self.db.root(), &arkinfo)?;

        // Delete every file listed in .arkinfo
        let protected_dirs: HashSet<PathBuf> = [
            "/usr",
            "/etc",
            "/bin",
            "/lib",
            "/var",
            "/usr/bin",
            "/usr/lib",
            "/usr/share",
            "/etc/arkpkg",
            "/etc/arkpkg/packages",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        let mut affected_dirs = HashSet::new();
        let mut removed_files: Vec<(PathBuf, PathBuf)> = Vec::new();

        for file_rel in &arkinfo.files {
            let target_path = self
                .db
                .root()
                .join(file_rel.strip_prefix("/").unwrap_or(file_rel));

            match fs::symlink_metadata(&target_path) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    if let Some(parent) = target_path.parent() {
                        affected_dirs.insert(parent.to_path_buf());
                    }
                    match backup_installed_file(self.db.root(), &target_path) {
                        Ok(Some(backup)) => {
                            self.logger
                                .action("REMOVE", &format!("{}", file_rel.display()));
                            removed_files.push((target_path, backup));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            restore_removed_files(&removed_files)?;
                            return Err(error);
                        }
                    }
                }
                Ok(_) => {
                    restore_removed_files(&removed_files)?;
                    return Err(ArkError::PackageConflict(format!(
                        "Refusing to remove non-file package path '{}'",
                        file_rel.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    restore_removed_files(&removed_files)?;
                    return Err(ArkError::Io(error));
                }
            }
        }

        let mut restored_preserved_files = Vec::new();
        for (original, backup) in &preserved_files {
            match fs::symlink_metadata(original) {
                Ok(_) => {
                    rollback_files(&restored_preserved_files, &removed_files)?;
                    return Err(ArkError::PackageConflict(format!(
                        "Cannot restore pre-install file '{}' because the path is occupied",
                        original.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    rollback_files(&restored_preserved_files, &removed_files)?;
                    return Err(ArkError::Io(error));
                }
            }
            if let Err(error) = fs::rename(backup, original) {
                rollback_files(&restored_preserved_files, &removed_files)?;
                return Err(ArkError::Io(error));
            }
            restored_preserved_files.push((original.clone(), backup.clone()));
        }

        // Remove database records only after all package files have been staged for deletion.
        self.logger.info("Updating package.db");
        if let Err(error) = self.db.remove_installed_package(pkg_name) {
            rollback_files(&restored_preserved_files, &removed_files)?;
            return Err(error);
        }

        self.logger.info("Updating file.db");
        if let Err(error) = self.db.remove_file_ownership(&arkinfo.files) {
            self.restore_database_and_files(
                pkg_name,
                &arkinfo,
                &removed_files,
                &restored_preserved_files,
                true,
                false,
            )?;
            return Err(error);
        }

        self.logger.info("Deleting .arkinfo metadata file");
        if let Err(error) = self.db.remove_arkinfo(pkg_name) {
            self.restore_database_and_files(
                pkg_name,
                &arkinfo,
                &removed_files,
                &restored_preserved_files,
                true,
                true,
            )?;
            return Err(error);
        }

        for (_, backup) in &removed_files {
            let _ = fs::remove_file(backup);
        }

        // Remove empty directories if they become empty (never system dirs).
        for dir in affected_dirs {
            let mut current = dir;
            while current.starts_with(self.db.root()) && current != self.db.root() {
                let relative = PathBuf::from("/")
                    .join(current.strip_prefix(self.db.root()).unwrap_or(&current));
                if protected_dirs.contains(&relative) {
                    break;
                }
                if let Ok(mut entries) = fs::read_dir(&current) {
                    if entries.next().is_none() {
                        self.logger
                            .action("REMOVE DIR", &format!("{}", relative.display()));
                        let _ = fs::remove_dir(&current);
                    } else {
                        break;
                    }
                } else {
                    break;
                }
                if let Some(parent) = current.parent().map(Path::to_path_buf) {
                    current = parent;
                } else {
                    break;
                }
            }
        }

        // 9 & 10. Log transaction and report success
        self.logger.info("DONE");
        Ok(())
    }

    fn restore_database_and_files(
        &self,
        pkg_name: &str,
        arkinfo: &crate::database::ArkInfo,
        removed_files: &[(PathBuf, PathBuf)],
        restored_preserved_files: &[(PathBuf, PathBuf)],
        restore_package: bool,
        restore_ownership: bool,
    ) -> Result<()> {
        if restore_ownership {
            self.db
                .add_file_ownership(&arkinfo.files, pkg_name)
                .map_err(|error| {
                    ArkError::Generic(format!("Failed to restore file ownership: {error}"))
                })?;
        }
        if restore_package {
            self.db
                .add_installed_package(pkg_name, &arkinfo.version)
                .map_err(|error| {
                    ArkError::Generic(format!("Failed to restore package record: {error}"))
                })?;
        }
        rollback_files(restored_preserved_files, removed_files)
    }
}

fn resolve_preserved_files(
    root: &Path,
    arkinfo: &crate::database::ArkInfo,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let canonical_root = root.canonicalize()?;
    let mut resolved = Vec::with_capacity(arkinfo.preserved_files.len());
    for (original, backup) in &arkinfo.preserved_files {
        let original_path = root.join(original.strip_prefix("/").map_err(|_| {
            ArkError::InvalidPackage(format!("Invalid preserved path '{}'", original.display()))
        })?);
        let backup_path = root.join(backup.strip_prefix("/").map_err(|_| {
            ArkError::InvalidPackage(format!("Invalid preserved backup '{}'", backup.display()))
        })?);
        let parent = original_path.parent().ok_or_else(|| {
            ArkError::InvalidPackage("Preserved path has no parent directory".into())
        })?;
        let canonical_parent = parent.canonicalize()?;
        if !canonical_parent.starts_with(&canonical_root) || backup_path.parent() != Some(parent) {
            return Err(ArkError::InvalidPackage(format!(
                "Preserved backup '{}' is outside the target root",
                backup.display()
            )));
        }
        let backup_metadata = fs::symlink_metadata(&backup_path)?;
        if !(backup_metadata.is_file() || backup_metadata.file_type().is_symlink()) {
            return Err(ArkError::PackageConflict(format!(
                "Preserved backup '{}' is not a file",
                backup.display()
            )));
        }
        resolved.push((original_path, backup_path));
    }
    Ok(resolved)
}

fn rollback_files(
    restored_preserved_files: &[(PathBuf, PathBuf)],
    removed_files: &[(PathBuf, PathBuf)],
) -> Result<()> {
    for (original, backup) in restored_preserved_files.iter().rev() {
        fs::rename(original, backup)?;
    }
    restore_removed_files(removed_files)
}

fn backup_installed_file(root: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ArkError::Io(error)),
    };
    if !(metadata.is_file() || metadata.file_type().is_symlink()) {
        return Err(ArkError::PackageConflict(format!(
            "Refusing to remove non-file package path '{}'",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ArkError::InvalidPackage("Installed path has no parent directory".into()))?;
    let canonical_root = root.canonicalize()?;
    let canonical_parent = parent.canonicalize()?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(ArkError::InvalidPackage(format!(
            "Removal path '{}' resolves outside target root",
            path.display()
        )));
    }
    let temporary = NamedTempFile::new_in(parent)?;
    fs::rename(path, temporary.path())?;
    let (_, backup) = temporary
        .keep()
        .map_err(|error| ArkError::Io(error.error))?;
    Ok(Some(backup))
}

fn restore_removed_files(removed_files: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (path, backup) in removed_files.iter().rev() {
        fs::rename(backup, path)?;
    }
    Ok(())
}
