use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Seek};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use tempfile::NamedTempFile;
use walkdir::WalkDir;

use crate::archive::extract_ark;
use crate::checksum::{compute_sha256_file, compute_sha256_reader};
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

        let temp_dir = crate::temp::directory()?;

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
        let mut file_ownership = self.db.read_file_ownership()?;

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
        let install_root = Dir::open_ambient_dir(self.db.root(), ambient_authority())?;
        let canonical_root = self.db.root().canonicalize()?;
        let mut destination_parents = HashMap::new();
        let mut file_changes: Vec<FileChange> = Vec::new();
        let mut installed_rel_files = Vec::new();
        let mut file_hashes = Vec::new();
        let mut preserved_files = Vec::new();

        for (src_path, rel_path) in &files_to_install {
            let relative_parent = rel_path.parent().unwrap();
            // Share one handle per destination directory across its files.
            let parent = match destination_parents.entry(relative_parent.to_path_buf()) {
                std::collections::hash_map::Entry::Occupied(entry) => Rc::clone(entry.get()),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let parent = match create_destination_parent(
                        &install_root,
                        &canonical_root,
                        relative_parent,
                    ) {
                        Ok(parent) => Rc::new(parent),
                        Err(error) => {
                            self.rollback(&file_changes);
                            return Err(error);
                        }
                    };
                    Rc::clone(entry.insert(parent))
                }
            };
            let name = PathBuf::from(rel_path.file_name().unwrap());

            let backup = match backup_existing_file(&parent, &name, temp_dir.path()) {
                Ok(backup) => backup,
                Err(error) => {
                    self.rollback(&file_changes);
                    return Err(error);
                }
            };
            file_changes.push(FileChange {
                parent,
                name,
                relative: rel_path.clone(),
                backup: backup.clone(),
            });
            let install_rel_path = PathBuf::from("/").join(rel_path);
            if let Some(backup_path) = backup.as_ref()
                && untracked_conflicts.contains(&install_rel_path)
            {
                let backup_rel_path = PathBuf::from("/").join(rel_path.with_file_name(backup_path));
                preserved_files.push((install_rel_path, backup_rel_path));
            }
            self.logger
                .action("COPY", &format!("/{}", rel_path.display()));

            let change = file_changes.last().unwrap();
            let file_hash =
                match install_package_file(src_path, &change.parent, &change.name, rel_path) {
                    Ok(hash) => hash,
                    Err(error) => {
                        self.rollback(&file_changes);
                        return Err(error);
                    }
                };
            let installed_path = PathBuf::from("/").join(rel_path);
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
        if let Err(error) = self.db.add_file_ownership_from_map(
            &mut file_ownership,
            &installed_rel_files,
            &metadata.name,
        ) {
            self.rollback(&file_changes);
            let _ = self.db.remove_arkinfo(&metadata.name);
            let _ = self.db.remove_installed_package(&metadata.name);
            return Err(error);
        }

        let preserved_backup_paths: HashSet<&PathBuf> =
            preserved_files.iter().map(|(_, backup)| backup).collect();
        for change in &file_changes {
            if let Some(backup) = &change.backup
                && !preserved_backup_paths
                    .contains(&PathBuf::from("/").join(change.relative.with_file_name(backup)))
            {
                let _ = change.parent.remove_file(backup);
            }
        }
        crate::linker_cache::refresh_if_needed(self.db.root(), &installed_rel_files, self.logger);
        self.logger.info("DONE");
        Ok(())
    }

    /// Restore pre-existing files and remove files from a failed installation.
    fn rollback(&self, file_changes: &[FileChange]) {
        for change in file_changes.iter().rev() {
            change.rollback();
        }
    }
}

// Keep the exact parent used for installation alive through cleanup or rollback.
struct FileChange {
    parent: Rc<Dir>,
    name: PathBuf,
    relative: PathBuf,
    backup: Option<PathBuf>,
}

impl FileChange {
    fn rollback(&self) {
        let _ = self.parent.remove_file(&self.name);
        if let Some(backup) = &self.backup {
            let _ = self.parent.rename(backup, &self.parent, &self.name);
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

fn create_destination_parent(
    root: &Dir,
    canonical_root: &Path,
    relative_parent: &Path,
) -> Result<Dir> {
    let mut current = canonical_root.to_path_buf();
    let mut directory = root.try_clone()?;

    for component in relative_parent.components() {
        let Component::Normal(name) = component else {
            return Err(ArkError::InvalidPackage(format!(
                "Unsafe install parent path '{}'",
                relative_parent.display()
            )));
        };
        current.push(name);

        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match directory.create_dir(name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(ArkError::Io(error)),
                }
                directory = directory.open_dir(name)?;
                continue;
            }
            Err(error) => return Err(ArkError::Io(error)),
        };

        if !metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Err(ArkError::PackageConflict(format!(
                "Install parent '{}' is not a directory",
                current.display()
            )));
        }

        let canonical_current = current.canonicalize()?;
        if !canonical_current.starts_with(canonical_root) {
            return Err(ArkError::InvalidPackage(format!(
                "Install destination '{}' resolves outside target root '{}'",
                canonical_current.display(),
                canonical_root.display()
            )));
        }
        // Canonicalization preserves existing in-root symlinks (including absolute
        // ones). It is only a name translation: Dir enforces containment again
        // while opening the translated path, even if an ancestor was replaced.
        let relative = canonical_current.strip_prefix(canonical_root).unwrap();
        directory = root.open_dir(if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        })?;
    }

    Ok(directory)
}

fn backup_existing_file(parent: &Dir, name: &Path, staging: &Path) -> Result<Option<PathBuf>> {
    let metadata = match parent.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ArkError::Io(error)),
    };
    if metadata.file_type().is_dir() {
        return Err(ArkError::PackageConflict(format!(
            "Cannot replace directory '{}' with a package file",
            name.display()
        )));
    }
    // Generate a random name in private staging, then reserve it exclusively in
    // the opened destination directory. Rename never follows the final symlink.
    let temporary = NamedTempFile::new_in(staging)?;
    let backup = PathBuf::from(temporary.path().file_name().unwrap());
    create_new_file(parent, &backup)?;
    if let Err(error) = parent.rename(name, parent, &backup) {
        let _ = parent.remove_file(&backup);
        return Err(ArkError::Io(error));
    }
    Ok(Some(backup))
}

fn create_new_file(parent: &Dir, name: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(parent.open_with(name, &options)?.into_std())
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

fn install_package_file(
    source: &Path,
    parent: &Dir,
    name: &Path,
    relative: &Path,
) -> Result<String> {
    if fs::symlink_metadata(source)?.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        if !safe_symlink_target(relative, &target) {
            return Err(ArkError::InvalidPackage(format!(
                "Symlink '{}' points outside the package root",
                relative.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            parent.symlink_contents(&target, name)?;
            Ok(crate::checksum::compute_sha256_bytes(
                target.as_os_str().as_bytes(),
            ))
        }
        #[cfg(not(unix))]
        return Err(ArkError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "package symlinks require a Unix host",
        )));
    } else {
        let mut source = fs::File::open(source)?;
        // Exclusive creation rejects an entry substituted after backup, including
        // a symlink or hard link. Copy, chmod and hash all use this same file.
        let mut destination = create_new_file(parent, name)?;
        io::copy(&mut source, &mut destination)?;
        destination.set_permissions(source.metadata()?.permissions())?;
        destination.rewind()?;
        Ok(compute_sha256_reader(destination)?)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::checksum::compute_sha256_bytes;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    #[test]
    fn replacing_the_root_path_cannot_redirect_directory_creation() {
        let container = TempDir::new().unwrap();
        let root = container.path().join("root");
        let moved_root = container.path().join("moved-root");
        let outside = TempDir::new().unwrap();
        fs::create_dir(&root).unwrap();
        let opened_root = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        fs::rename(&root, &moved_root).unwrap();
        symlink(outside.path(), &root).unwrap();

        create_destination_parent(&opened_root, &root, Path::new("new/deep")).unwrap();

        assert!(moved_root.join("new/deep").is_dir());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn ancestor_replacement_cannot_redirect_backup_copy_or_rollback() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        fs::create_dir(outside.path().join("bin")).unwrap();
        fs::write(root.path().join("usr/bin/tool"), b"original").unwrap();
        fs::write(outside.path().join("bin/tool"), b"outside").unwrap();
        let source = staging.path().join("payload");
        fs::write(&source, b"package").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();
        let opened_root = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let parent =
            create_destination_parent(&opened_root, root.path(), Path::new("usr/bin")).unwrap();

        // Deterministically replace the checked ancestor before the actual I/O.
        fs::rename(root.path().join("usr"), root.path().join("original-usr")).unwrap();
        symlink(outside.path(), root.path().join("usr")).unwrap();
        let name = PathBuf::from("tool");
        let backup = backup_existing_file(&parent, &name, staging.path()).unwrap();
        let hash =
            install_package_file(&source, &parent, &name, Path::new("usr/bin/tool")).unwrap();
        assert_eq!(hash, compute_sha256_bytes(b"package"));
        let installed = root.path().join("original-usr/bin/tool");
        assert_eq!(fs::read(&installed).unwrap(), b"package");
        assert_eq!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
            0o751
        );

        let change = FileChange {
            parent: Rc::new(parent),
            name,
            relative: PathBuf::from("usr/bin/tool"),
            backup,
        };
        change.rollback();
        assert_eq!(fs::read(installed).unwrap(), b"original");
        assert_eq!(
            fs::read(outside.path().join("bin/tool")).unwrap(),
            b"outside"
        );
        assert_eq!(fs::read_dir(outside.path().join("bin")).unwrap().count(), 1);
        assert_eq!(
            fs::read_dir(root.path().join("original-usr/bin"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn substituted_final_symlinks_and_hard_links_are_not_written() {
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let outside = staging.path().join("outside");
        let source = staging.path().join("payload");
        fs::write(&outside, b"outside").unwrap();
        fs::write(&source, b"package").unwrap();
        let parent = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let name = Path::new("tool");

        // These entries model substitutions after backup, before file creation.
        symlink(&outside, root.path().join(name)).unwrap();
        assert!(install_package_file(&source, &parent, name, name).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        parent.remove_file(name).unwrap();
        fs::hard_link(&outside, root.path().join(name)).unwrap();
        assert!(install_package_file(&source, &parent, name, name).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn ancestor_replacement_cannot_redirect_symlink_install_or_backup_cleanup() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        fs::create_dir(root.path().join("bin")).unwrap();
        fs::write(root.path().join("bin/tool"), b"original").unwrap();
        let opened_root = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let parent =
            create_destination_parent(&opened_root, root.path(), Path::new("bin")).unwrap();
        let backup = backup_existing_file(&parent, Path::new("tool"), staging.path())
            .unwrap()
            .unwrap();
        fs::write(outside.path().join(&backup), b"outside backup").unwrap();
        let source = staging.path().join("payload");
        symlink("/usr/lib/tool", &source).unwrap();

        fs::rename(root.path().join("bin"), root.path().join("original-bin")).unwrap();
        symlink(outside.path(), root.path().join("bin")).unwrap();
        assert_eq!(
            install_package_file(&source, &parent, Path::new("tool"), Path::new("bin/tool"))
                .unwrap(),
            compute_sha256_bytes(b"/usr/lib/tool")
        );
        assert_eq!(
            fs::read_link(root.path().join("original-bin/tool")).unwrap(),
            Path::new("/usr/lib/tool")
        );
        parent.remove_file(&backup).unwrap();
        assert!(!root.path().join("original-bin").join(&backup).exists());
        assert_eq!(
            fs::read(outside.path().join(backup)).unwrap(),
            b"outside backup"
        );
        assert!(!outside.path().join("tool").exists());
    }
}
