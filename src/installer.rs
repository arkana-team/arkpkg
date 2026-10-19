use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

use crate::archive::extract_ark;
use crate::checksum::{compute_sha256_file, compute_sha256_path};
use crate::database::{ArkInfo, Database};
use crate::errors::{ArkError, Result};
use crate::logger::Logger;
use crate::prompt::PromptHandler;

pub struct Installer<'a> {
    db: &'a Database,
    logger: &'a Logger,
    prompt: PromptHandler,
}

impl<'a> Installer<'a> {
    pub fn new(db: &'a Database, logger: &'a Logger, prompt: PromptHandler) -> Self {
        Self { db, logger, prompt }
    }

    /// Executes the 18-step package installation sequence.
    pub fn install<P: AsRef<Path>>(&mut self, ark_path: P) -> Result<()> {
        let ark_path = ark_path.as_ref();

        // 1 & 2. Read and verify .ark package path
        self.logger
            .info(&format!("Reading package {}", ark_path.display()));
        if !ark_path.exists() {
            return Err(ArkError::PackageNotFound(format!(
                "Package file '{}' not found",
                ark_path.display()
            )));
        }

        let temp_dir = tempfile::TempDir::new()?;

        // 3, 4, 5. Decompress, read & validate metadata
        self.logger
            .info("Decompressing archive and reading metadata");
        let metadata = extract_ark(ark_path, temp_dir.path())?;

        // 6. Ensure package architecture matches current system
        self.logger.info("Checking package architecture");
        let current_arch = std::env::consts::ARCH;
        if metadata.arch != "any" && metadata.arch != "all" && metadata.arch != current_arch {
            return Err(ArkError::ArchitectureMismatch {
                package_arch: metadata.arch.clone(),
                system_arch: current_arch.to_string(),
            });
        }

        // 7. Check if already installed
        self.logger.info("Checking if package is already installed");
        if let Some(installed_ver) = self.db.is_installed(&metadata.name)? {
            if installed_ver == metadata.version {
                return Err(ArkError::PackageAlreadyInstalled(format!(
                    "Package '{}' version {} is already installed",
                    metadata.name, installed_ver
                )));
            }
            return Err(ArkError::PackageConflict(format!(
                "Package '{}' is already installed at version {}; remove it before installing {}",
                metadata.name, installed_ver, metadata.version
            )));
        }

        // 8. Verify dependency requirements against installed packages and provides
        self.logger.info("Checking dependencies");
        let installed_pkgs = self.db.read_installed_packages()?;
        for dep_req in &metadata.dependencies {
            let mut satisfied = false;
            for (inst_name, inst_ver) in &installed_pkgs {
                if inst_name.eq_ignore_ascii_case(&dep_req.name)
                    && dep_req.is_satisfied_by(inst_ver)
                {
                    satisfied = true;
                    break;
                }
                // Check if installed package provides the required feature
                if let Ok(inst_arkinfo) = self.db.read_arkinfo(inst_name) {
                    for provide in &inst_arkinfo.provides {
                        if provide.name == dep_req.name
                            && dep_req.is_satisfied_by(&inst_arkinfo.version)
                        {
                            satisfied = true;
                            break;
                        }
                    }
                }
                if satisfied {
                    break;
                }
            }
            if !satisfied {
                return Err(ArkError::MissingDependency(format!(
                    "Unsatisfied dependency: {}",
                    dep_req
                )));
            }
        }

        // 9. Check for package conflicts
        self.logger.info("Checking package conflicts");
        for conflict in &metadata.conflicts {
            if self.db.is_installed(conflict)?.is_some() {
                return Err(ArkError::PackageConflict(format!(
                    "Conflicts with installed package '{}'",
                    conflict
                )));
            }
            for (installed_name, _) in &installed_pkgs {
                match self.db.read_arkinfo(installed_name) {
                    Ok(installed_info)
                        if installed_info
                            .provides
                            .iter()
                            .any(|provide| provide.name.eq_ignore_ascii_case(conflict)) =>
                    {
                        return Err(ArkError::PackageConflict(format!(
                            "Conflicts with '{}' provided by installed package '{}'",
                            conflict, installed_name
                        )));
                    }
                    _ => {}
                }
            }
        }
        for (installed_name, _) in &installed_pkgs {
            if let Ok(installed_info) = self.db.read_arkinfo(installed_name) {
                if installed_info
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.eq_ignore_ascii_case(&metadata.name))
                {
                    return Err(ArkError::PackageConflict(format!(
                        "Installed package '{}' conflicts with '{}'",
                        installed_name, metadata.name
                    )));
                }
                for provide in &metadata.provides {
                    if installed_info
                        .conflicts
                        .iter()
                        .any(|conflict| conflict.eq_ignore_ascii_case(&provide.name))
                    {
                        return Err(ArkError::PackageConflict(format!(
                            "Installed package '{}' conflicts with '{}' provided by '{}'",
                            installed_name, provide.name, metadata.name
                        )));
                    }
                }
            }
        }

        // 10 & 11. Check for file conflicts and prompt user
        self.logger.info("Checking file conflicts");
        let pkg_files_root = temp_dir.path().join("package");
        let file_ownership = self.db.read_file_ownership()?;

        let mut files_to_install = Vec::new();
        let mut conflicts_to_prompt = Vec::new();

        for entry in WalkDir::new(&pkg_files_root).sort_by_file_name() {
            let entry = entry.map_err(|error| {
                ArkError::Generic(format!("Could not inspect package files: {error}"))
            })?;
            let relative = entry
                .path()
                .strip_prefix(&pkg_files_root)
                .map_err(|error| ArkError::Generic(format!("Invalid package path: {error}")))?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(ArkError::InvalidPackage(format!(
                    "Unsafe package file path '{}'",
                    relative.display()
                )));
            }

            let target_path = self.db.root().join(relative);

            if entry.file_type().is_file() || entry.file_type().is_symlink() {
                files_to_install.push((entry.path().to_path_buf(), relative.to_path_buf()));

                let install_rel_path = PathBuf::from("/").join(relative);
                if path_exists(&target_path)? {
                    if let Some(owner) = file_ownership.get(&install_rel_path) {
                        if owner != &metadata.name {
                            return Err(ArkError::PackageConflict(format!(
                                "File '{}' is already owned by package '{}'",
                                install_rel_path.display(),
                                owner
                            )));
                        }
                    } else {
                        conflicts_to_prompt
                            .push((install_rel_path.clone(), "untracked".to_string()));
                    }
                }
            }
        }

        for (conf_file, owner) in &conflicts_to_prompt {
            let prompt_msg = format!(
                "The file already exists:\n{}\nOwned by package:\n{}\nOverwrite?",
                conf_file.display(),
                owner
            );
            if !self.prompt.ask(&prompt_msg)? {
                return Err(ArkError::UserCancelled);
            }
        }
        let untracked_conflicts: HashSet<PathBuf> = conflicts_to_prompt
            .iter()
            .filter(|(_, owner)| owner == "untracked")
            .map(|(path, _)| path.clone())
            .collect();

        // 12. Install files with backups so failures preserve overwritten files.
        self.logger.info("Copying files into installation root");
        let pkg_checksum = compute_sha256_file(ark_path)?;
        let mut file_changes: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        let mut installed_rel_files = Vec::new();
        let mut file_hashes = Vec::new();
        let mut preserved_files = Vec::new();

        for (src_path, rel_path) in &files_to_install {
            let dest_path = self.db.root().join(rel_path);
            if let Some(parent) = dest_path.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    self.rollback(&file_changes);
                    return Err(ArkError::Io(error));
                }
                if let Err(error) = validate_destination_parent(self.db.root(), parent) {
                    self.rollback(&file_changes);
                    return Err(error);
                }
            }

            let backup = match backup_existing_file(&dest_path) {
                Ok(backup) => backup,
                Err(error) => {
                    self.rollback(&file_changes);
                    return Err(error);
                }
            };
            file_changes.push((dest_path.clone(), backup.clone()));
            let install_rel_path = PathBuf::from("/").join(rel_path);
            if let Some(backup_path) = backup.as_ref()
                && untracked_conflicts.contains(&install_rel_path)
            {
                let backup_rel_path = match backup_path.strip_prefix(self.db.root()) {
                    Ok(path) => PathBuf::from("/").join(path),
                    Err(error) => {
                        self.rollback(&file_changes);
                        return Err(ArkError::Generic(format!(
                            "Could not record overwritten file backup: {error}"
                        )));
                    }
                };
                preserved_files.push((install_rel_path, backup_rel_path));
            }
            self.logger
                .action("COPY", &format!("/{}", rel_path.display()));

            if let Err(error) = install_package_file(src_path, &dest_path, rel_path) {
                self.rollback(&file_changes);
                return Err(error);
            }
            let installed_path = PathBuf::from("/").join(rel_path);
            let file_hash = match compute_sha256_path(&dest_path) {
                Ok(hash) => hash,
                Err(error) => {
                    self.rollback(&file_changes);
                    return Err(ArkError::Io(error));
                }
            };
            file_hashes.push((installed_path.clone(), file_hash));
            installed_rel_files.push(installed_path);
        }

        // 13. Generate .arkinfo file
        self.logger.info("Writing package info metadata (.arkinfo)");
        let arkinfo = ArkInfo {
            name: metadata.name.clone(),
            version: metadata.version.clone(),
            arch: metadata.arch.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            checksum: pkg_checksum,
            files: installed_rel_files.clone(),
            file_hashes,
            dependencies: metadata.dependencies.clone(),
            provides: metadata.provides.clone(),
            conflicts: metadata.conflicts.clone(),
            preserved_files: preserved_files.clone(),
        };

        if let Err(error) = self.db.write_arkinfo(&arkinfo) {
            self.rollback(&file_changes);
            return Err(error);
        }

        // 14. Update package.db
        self.logger.info("Updating package database (package.db)");
        if let Err(error) = self
            .db
            .add_installed_package(&metadata.name, &metadata.version)
        {
            self.rollback(&file_changes);
            let _ = self.db.remove_arkinfo(&metadata.name);
            return Err(error);
        }

        // 15. Update file.db
        self.logger.info("Updating file database (file.db)");
        if let Err(error) = self
            .db
            .add_file_ownership(&installed_rel_files, &metadata.name)
        {
            self.rollback(&file_changes);
            let _ = self.db.remove_arkinfo(&metadata.name);
            let _ = self.db.remove_installed_package(&metadata.name);
            return Err(error);
        }

        let preserved_backup_paths: HashSet<PathBuf> = preserved_files
            .iter()
            .map(|(_, backup)| {
                self.db
                    .root()
                    .join(backup.strip_prefix("/").unwrap_or(backup))
            })
            .collect();
        for (_, backup) in &file_changes {
            if let Some(path) = backup
                && !preserved_backup_paths.contains(path)
            {
                let _ = fs::remove_file(path);
            }
        }
        self.logger.info("DONE");
        Ok(())
    }

    /// Restore pre-existing files and remove files from a failed installation.
    fn rollback(&self, file_changes: &[(PathBuf, Option<PathBuf>)]) {
        for (path, backup) in file_changes.iter().rev() {
            if path_exists(path).unwrap_or(false) {
                let _ = fs::remove_file(path);
            }
            if let Some(backup_path) = backup {
                let _ = fs::rename(backup_path, path);
            }
        }
    }
}

fn path_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn validate_destination_parent(root: &Path, parent: &Path) -> Result<()> {
    let canonical_root = root.canonicalize()?;
    let canonical_parent = parent.canonicalize()?;
    if canonical_parent.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(ArkError::InvalidPackage(format!(
            "Install destination '{}' resolves outside target root '{}'",
            canonical_parent.display(),
            canonical_root.display()
        )))
    }
}

fn backup_existing_file(path: &Path) -> Result<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ArkError::Io(error)),
    };
    if metadata.file_type().is_dir() {
        return Err(ArkError::PackageConflict(format!(
            "Cannot replace directory '{}' with a package file",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ArkError::InvalidPackage("Install path has no parent directory".into()))?;
    let temporary = NamedTempFile::new_in(parent)?;
    fs::rename(path, temporary.path())?;
    let (_, backup_path) = temporary
        .keep()
        .map_err(|error| ArkError::Io(error.error))?;
    Ok(Some(backup_path))
}

fn safe_symlink_target(link_path: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return target
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)));
    }

    let mut depth = link_path
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
            Component::CurDir => {}
        }
    }
    true
}

fn install_package_file(source: &Path, destination: &Path, relative: &Path) -> Result<()> {
    if fs::symlink_metadata(source)?.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        if !safe_symlink_target(relative, &target) {
            return Err(ArkError::InvalidPackage(format!(
                "Symlink '{}' points outside the package root",
                relative.display()
            )));
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, destination)?;
        #[cfg(not(unix))]
        return Err(ArkError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "package symlinks require a Unix host",
        )));
    } else {
        fs::copy(source, destination)?;
    }
    Ok(())
}
