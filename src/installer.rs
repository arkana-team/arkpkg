use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::MetadataExt;
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
use crate::database::{ArkInfo, Database, validate_installed_path, validate_payload_path};
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
        let canonical_root = self.db.root().canonicalize()?;
        // Normalize legacy ownership records too, without following the final symlink.
        let mut resolved_ownership = HashMap::new();
        for (path, owner) in &file_ownership {
            let destination = resolve_destination(
                &canonical_root,
                path.strip_prefix("/").unwrap_or(path),
                false,
            )?;
            if let Some(previous) = resolved_ownership.insert(destination, owner.clone())
                && previous != *owner
            {
                return Err(ArkError::PackageConflict(format!(
                    "Multiple packages own the destination of '{}'",
                    path.display()
                )));
            }
        }

        let mut destinations = HashSet::new();
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

            let install_rel_path = PathBuf::from("/").join(relative);
            validate_installed_path(&install_rel_path)?;

            if entry.file_type().is_file() || entry.file_type().is_symlink() {
                validate_payload_path(relative)?;
                let destination = resolve_destination(&canonical_root, relative, false)?;
                let resolved_relative =
                    destination.strip_prefix(&canonical_root).map_err(|_| {
                        ArkError::InvalidPackage(
                            "Install destination resolves outside target root".into(),
                        )
                    })?;
                validate_installed_path(&PathBuf::from("/").join(resolved_relative))?;
                validate_payload_path(resolved_relative)?;
                validate_database_destination(&canonical_root, &destination)?;
                if !destinations.insert(destination.clone()) {
                    return Err(ArkError::InvalidPackage(format!(
                        "Multiple package files resolve to '{}'",
                        destination.display()
                    )));
                }
                let install_rel_path =
                    PathBuf::from("/").join(destination.strip_prefix(&canonical_root).unwrap());
                if check_file_conflict(
                    &destination,
                    &install_rel_path,
                    &resolved_ownership,
                    &metadata.name,
                )? {
                    conflicts_to_prompt.push((install_rel_path, "untracked".to_string()));
                }
                files_to_install.push((
                    entry.path().to_path_buf(),
                    relative.to_path_buf(),
                    destination,
                ));
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
        let mut untracked_conflicts: HashSet<PathBuf> = conflicts_to_prompt
            .iter()
            .filter(|(_, owner)| owner == "untracked")
            .map(|(path, _)| path.clone())
            .collect();

        // 12. Install files with backups so failures preserve overwritten files.
        self.logger.info("Copying files into installation root");
        let pkg_checksum = compute_sha256_file(ark_path)?;
        let install_root = Dir::open_ambient_dir(&canonical_root, ambient_authority())?;
        let mut destination_parents = ParentDirectoryCache::default();
        let mut file_changes: Vec<FileChange> = Vec::new();
        let mut installed_rel_files = Vec::new();
        let mut file_hashes = Vec::new();
        let mut preserved_files = Vec::new();

        for (src_path, rel_path, planned_destination) in &files_to_install {
            let prepared = (|| -> Result<_> {
                let destination = resolve_destination(&canonical_root, rel_path, true)?;
                let resolved_relative =
                    destination.strip_prefix(&canonical_root).map_err(|_| {
                        ArkError::InvalidPackage(
                            "Install destination resolves outside target root".into(),
                        )
                    })?;
                validate_payload_path(resolved_relative)?;
                validate_database_destination(&canonical_root, &destination)?;
                if destination != *planned_destination {
                    return Err(ArkError::InvalidPackage(format!(
                        "Install destination for '{}' changed during installation",
                        rel_path.display()
                    )));
                }
                let installed_path =
                    PathBuf::from("/").join(destination.strip_prefix(&canonical_root).unwrap());
                // Recheck the actual destination immediately before backing it up.
                let untracked = check_file_conflict(
                    &destination,
                    &installed_path,
                    &resolved_ownership,
                    &metadata.name,
                )?;
                if untracked && !untracked_conflicts.contains(&installed_path) {
                    if !self.prompt.ask(&format!(
                        "The file already exists:\n{}\nOwned by package:\nuntracked\nOverwrite?",
                        installed_path.display()
                    ))? {
                        return Err(ArkError::UserCancelled);
                    }
                    untracked_conflicts.insert(installed_path.clone());
                }
                Ok((destination, installed_path, untracked))
            })();
            let (dest_path, install_rel_path, untracked) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.rollback(&file_changes, &install_root, &mut destination_parents);
                    return Err(error);
                }
            };

            let relative_destination = match dest_path.strip_prefix(&canonical_root) {
                Ok(path) => path,
                Err(error) => {
                    self.rollback(&file_changes, &install_root, &mut destination_parents);
                    return Err(ArkError::Generic(format!(
                        "Could not resolve install destination relative to target root: {error}"
                    )));
                }
            };
            let relative_parent = match relative_destination.parent() {
                Some(parent) => parent,
                None => {
                    self.rollback(&file_changes, &install_root, &mut destination_parents);
                    return Err(ArkError::InvalidPackage(
                        "Install destination has no parent directory".into(),
                    ));
                }
            };
            let (parent, parent_identity) = match destination_parents.open_or_create(
                &install_root,
                &canonical_root,
                relative_parent,
            ) {
                Ok(parent) => parent,
                Err(error) => {
                    self.rollback(&file_changes, &install_root, &mut destination_parents);
                    return Err(error);
                }
            };
            let name = PathBuf::from(relative_destination.file_name().unwrap());

            let backup = match backup_existing_file(&parent, &name, temp_dir.path()) {
                Ok(backup) => backup,
                Err(error) => {
                    self.rollback(&file_changes, &install_root, &mut destination_parents);
                    return Err(error);
                }
            };
            file_changes.push(FileChange {
                parent_path: relative_parent.to_path_buf(),
                parent_identity,
                name: name.clone(),
                relative: install_rel_path.clone(),
                backup: backup.clone(),
            });
            if let Some(backup_name) = backup.as_ref()
                && untracked
            {
                let backup_rel_path = PathBuf::from("/").join(relative_parent).join(backup_name);
                preserved_files.push((install_rel_path.clone(), backup_rel_path));
            }
            self.logger
                .action("COPY", &format!("/{}", rel_path.display()));

            let resolved_relative = install_rel_path.strip_prefix("/").unwrap();
            let file_hash = match install_package_file(src_path, &parent, &name, resolved_relative)
            {
                Ok(hash) => hash,
                Err(error) => {
                    self.rollback(&file_changes, &install_root, &mut destination_parents);
                    return Err(error);
                }
            };
            file_hashes.push((install_rel_path.clone(), file_hash));
            installed_rel_files.push(install_rel_path);
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
            self.rollback(&file_changes, &install_root, &mut destination_parents);
            return Err(error);
        }

        // 14. Update package.db
        self.logger.info("Updating package database (package.db)");
        if let Err(error) = self
            .db
            .add_installed_package(&metadata.name, &metadata.version)
        {
            self.rollback(&file_changes, &install_root, &mut destination_parents);
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
            self.rollback(&file_changes, &install_root, &mut destination_parents);
            let _ = self.db.remove_arkinfo(&metadata.name);
            let _ = self.db.remove_installed_package(&metadata.name);
            return Err(error);
        }

        let preserved_backup_paths: HashSet<PathBuf> = preserved_files
            .iter()
            .map(|(_, backup)| backup.clone())
            .collect();
        for change in &file_changes {
            if let Some(backup) = &change.backup
                && !preserved_backup_paths.contains(&change.relative.with_file_name(backup))
                && let Ok(parent) = change.open_parent(&install_root, &mut destination_parents)
            {
                let _ = parent.remove_file(backup);
            }
        }
        crate::linker_cache::refresh_if_needed(self.db.root(), &installed_rel_files, self.logger);
        self.logger.info("DONE");
        Ok(())
    }

    /// Restore pre-existing files and remove files from a failed installation.
    fn rollback(
        &self,
        file_changes: &[FileChange],
        root: &Dir,
        parent_cache: &mut ParentDirectoryCache,
    ) {
        for change in file_changes.iter().rev() {
            if let Err(error) = change.rollback(root, parent_cache) {
                self.logger.info(&format!(
                    "Rollback could not restore '{}': {error}",
                    change.relative.display()
                ));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

fn directory_identity(dir: &Dir) -> Result<DirectoryIdentity> {
    let metadata = dir.dir_metadata()?;
    #[cfg(unix)]
    {
        Ok(DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(ArkError::Generic(
            "Directory identity checks require a Unix host".into(),
        ))
    }
}

#[derive(Default)]
struct ParentDirectoryCache {
    last: Option<(PathBuf, Rc<Dir>, DirectoryIdentity)>,
}

impl ParentDirectoryCache {
    fn open_or_create(
        &mut self,
        root: &Dir,
        canonical_root: &Path,
        relative: &Path,
    ) -> Result<(Rc<Dir>, DirectoryIdentity)> {
        if let Some((path, dir, identity)) = &self.last
            && path == relative
        {
            return Ok((Rc::clone(dir), *identity));
        }
        let dir = Rc::new(create_destination_parent(root, canonical_root, relative)?);
        let identity = directory_identity(&dir)?;
        self.last = Some((relative.to_path_buf(), Rc::clone(&dir), identity));
        Ok((dir, identity))
    }

    fn open_matching(
        &mut self,
        root: &Dir,
        relative: &Path,
        expected: DirectoryIdentity,
    ) -> Result<Rc<Dir>> {
        if let Some((path, dir, identity)) = &self.last
            && path == relative
        {
            if *identity != expected {
                return Err(ArkError::InvalidPackage(
                    "Install parent changed during transaction".into(),
                ));
            }
            return Ok(Rc::clone(dir));
        }
        let dir = Rc::new(if relative.as_os_str().is_empty() {
            root.try_clone()?
        } else {
            root.open_dir(relative)?
        });
        let identity = directory_identity(&dir)?;
        if identity != expected {
            return Err(ArkError::InvalidPackage(
                "Install parent changed during transaction".into(),
            ));
        }
        self.last = Some((relative.to_path_buf(), Rc::clone(&dir), identity));
        Ok(dir)
    }
}

struct FileChange {
    parent_path: PathBuf,
    parent_identity: DirectoryIdentity,
    name: PathBuf,
    relative: PathBuf,
    backup: Option<PathBuf>,
}

impl FileChange {
    fn open_parent(&self, root: &Dir, parent_cache: &mut ParentDirectoryCache) -> Result<Rc<Dir>> {
        parent_cache.open_matching(root, &self.parent_path, self.parent_identity)
    }

    fn rollback(&self, root: &Dir, parent_cache: &mut ParentDirectoryCache) -> Result<()> {
        let parent = self.open_parent(root, parent_cache)?;
        match parent.remove_file(&self.name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ArkError::Io(error)),
        }
        if let Some(backup) = &self.backup {
            parent.rename(backup, &parent, &self.name)?;
        }
        Ok(())
    }
}

fn path_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Resolve only parent components: a final symlink is replaced, not followed.
/// Missing parents can be planned without creating them during conflict checks.
fn resolve_destination(root: &Path, relative: &Path, create_parent: bool) -> Result<PathBuf> {
    let parent = relative
        .parent()
        .ok_or_else(|| ArkError::InvalidPackage("Install path has no parent directory".into()))?;
    let mut current = root.to_path_buf();

    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(ArkError::InvalidPackage(format!(
                "Unsafe install parent path '{}'",
                parent.display()
            )));
        };
        current.push(name);

        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !create_parent {
                    continue;
                }
                match fs::create_dir(&current) {
                    Ok(()) => fs::symlink_metadata(&current)?,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        fs::symlink_metadata(&current)?
                    }
                    Err(error) => return Err(ArkError::Io(error)),
                }
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
        if !canonical_current.starts_with(root) {
            return Err(ArkError::InvalidPackage(format!(
                "Install destination '{}' resolves outside target root '{}'",
                canonical_current.display(),
                root.display()
            )));
        }
        if !canonical_current.is_dir() {
            return Err(ArkError::PackageConflict(format!(
                "Install parent '{}' does not resolve to a directory",
                current.display()
            )));
        }
        current = canonical_current;
    }

    let name = relative
        .file_name()
        .ok_or_else(|| ArkError::InvalidPackage("Install path has no file name".into()))?;
    Ok(current.join(name))
}

/// Reject resolved destinations that overlap package-manager state, including
/// paths that reach it through supported in-root directory symlinks.
fn validate_database_destination(root: &Path, destination: &Path) -> Result<()> {
    for relative in [
        "etc/arkpkg/package.db",
        "etc/arkpkg/file.db",
        "etc/arkpkg/packages",
    ] {
        let mut reserved = resolve_destination(root, Path::new(relative), false)?;
        if relative == "etc/arkpkg/packages" {
            match fs::symlink_metadata(&reserved) {
                Ok(_) => {
                    reserved = reserved.canonicalize().map_err(|error| {
                        ArkError::InvalidPackage(format!(
                            "Reserved package metadata path cannot be resolved: {error}"
                        ))
                    })?;
                    if !reserved.starts_with(root) {
                        return Err(ArkError::InvalidPackage(format!(
                            "Reserved package metadata path '{}' resolves outside the target root",
                            reserved.display()
                        )));
                    }
                    if !reserved.is_dir() {
                        return Err(ArkError::InvalidPackage(
                            "Reserved package metadata path is not a directory".into(),
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(ArkError::Io(error)),
            }
        }
        if destination.starts_with(&reserved) || reserved.starts_with(destination) {
            return Err(ArkError::InvalidPackage(format!(
                "Package file '{}' overlaps reserved database state",
                destination.display()
            )));
        }
    }
    Ok(())
}

/// Returns whether overwriting this destination requires consent and preservation.
fn check_file_conflict(
    destination: &Path,
    installed_path: &Path,
    ownership: &HashMap<PathBuf, String>,
    package: &str,
) -> Result<bool> {
    if let Some(owner) = ownership.get(destination) {
        if owner != package {
            return Err(ArkError::PackageConflict(format!(
                "File '{}' is already owned by package '{}'",
                installed_path.display(),
                owner
            )));
        }
        return Ok(false);
    }
    Ok(path_exists(destination)?)
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
                // cap-std resolves this relative to the opened parent and keeps
                // the lookup beneath it, even if the name changed after create_dir.
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
    let temporary = NamedTempFile::new_in(staging)?;
    let backup = PathBuf::from(
        temporary
            .path()
            .file_name()
            .ok_or_else(|| ArkError::Generic("Temporary backup has no file name".into()))?,
    );
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
            Ok(format!(
                "symlink-sha256={}",
                crate::checksum::compute_sha256_bytes(target.as_os_str().as_bytes())
            ))
        }
        #[cfg(not(unix))]
        return Err(ArkError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "package symlinks require a Unix host",
        )));
    } else {
        let mut source = fs::File::open(source)?;
        // Exclusive creation rejects a substituted symlink or hard link. Copy,
        // permissions, and integrity hashing all use this opened file.
        let mut destination = create_new_file(parent, name)?;
        io::copy(&mut source, &mut destination)?;
        destination.set_permissions(source.metadata()?.permissions())?;
        destination.rewind()?;
        Ok(format!(
            "file-sha256={}",
            compute_sha256_reader(destination)?
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::checksum::compute_sha256_bytes;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    #[test]
    fn opened_parent_keeps_writes_and_rollback_inside_original_directory() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        fs::create_dir_all(outside.path().join("bin")).unwrap();
        fs::write(root.path().join("usr/bin/tool"), b"original").unwrap();
        fs::write(outside.path().join("bin/tool"), b"outside").unwrap();
        let source = staging.path().join("payload");
        fs::write(&source, b"package").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();

        let canonical_root = root.path().canonicalize().unwrap();
        let opened_root = Dir::open_ambient_dir(&canonical_root, ambient_authority()).unwrap();
        let mut parent_cache = ParentDirectoryCache::default();
        let (parent, parent_identity) = parent_cache
            .open_or_create(&opened_root, &canonical_root, Path::new("usr/bin"))
            .unwrap();

        fs::rename(root.path().join("usr"), root.path().join("original-usr")).unwrap();
        symlink(outside.path(), root.path().join("usr")).unwrap();
        let name = PathBuf::from("tool");
        let backup = backup_existing_file(&parent, &name, staging.path()).unwrap();
        let hash =
            install_package_file(&source, &parent, &name, Path::new("usr/bin/tool")).unwrap();

        assert_eq!(
            hash,
            format!("file-sha256={}", compute_sha256_bytes(b"package"))
        );
        let installed = root.path().join("original-usr/bin/tool");
        assert_eq!(fs::read(&installed).unwrap(), b"package");
        assert_eq!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(
            fs::read(outside.path().join("bin/tool")).unwrap(),
            b"outside"
        );

        FileChange {
            parent_path: PathBuf::from("usr/bin"),
            parent_identity,
            name,
            relative: PathBuf::from("/usr/bin/tool"),
            backup,
        }
        .rollback(&opened_root, &mut parent_cache)
        .unwrap();

        assert_eq!(fs::read(installed).unwrap(), b"original");
        assert_eq!(
            fs::read(outside.path().join("bin/tool")).unwrap(),
            b"outside"
        );
        assert_eq!(fs::read_dir(outside.path().join("bin")).unwrap().count(), 1);
    }

    #[test]
    fn exclusive_install_rejects_substituted_symlink_and_hard_link() {
        let root = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let outside = staging.path().join("outside");
        let source = staging.path().join("payload");
        fs::write(&outside, b"outside").unwrap();
        fs::write(&source, b"package").unwrap();
        let parent = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let name = Path::new("tool");

        symlink(&outside, root.path().join(name)).unwrap();
        assert!(install_package_file(&source, &parent, name, name).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        parent.remove_file(name).unwrap();

        fs::hard_link(&outside, root.path().join(name)).unwrap();
        assert!(install_package_file(&source, &parent, name, name).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn installed_symlink_hash_keeps_its_integrity_type() {
        let root = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        let link = source.path().join("tool-link");
        symlink("/usr/lib/tool", &link).unwrap();
        let parent = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let hash =
            install_package_file(&link, &parent, Path::new("tool"), Path::new("bin/tool")).unwrap();

        assert_eq!(
            hash,
            format!("symlink-sha256={}", compute_sha256_bytes(b"/usr/lib/tool"))
        );
        assert_eq!(
            fs::read_link(root.path().join("tool")).unwrap(),
            Path::new("/usr/lib/tool")
        );
    }

    #[test]
    fn install_and_remove_support_more_parent_directories_than_the_fd_limit() {
        const DIRECTORY_COUNT: usize = 1_100;
        let root = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        let package_root = source.path().join("package/usr/share/fd-limit");
        fs::create_dir_all(&package_root).unwrap();
        fs::write(
            source.path().join("ARKPKG"),
            "Name: fd-limit-integration\nDescription: exercise bounded parent handles\nVersion: 1.0.0\nArch: any\n",
        )
        .unwrap();
        for index in 0..DIRECTORY_COUNT {
            let directory = package_root.join(format!("d{index:04}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("payload"), b"fd-limit-test").unwrap();
        }
        let archive = source.path().join("fd-limit-integration.ark");
        crate::archive::create_ark(source.path(), &archive).unwrap();

        let database = Database::new(root.path());
        database.init().unwrap();
        let logger = Logger::new(root.path());
        logger.init().unwrap();
        Installer::new(&database, &logger, PromptHandler::new(true, false, false))
            .install(&archive)
            .unwrap();
        assert!(
            database
                .is_installed("fd-limit-integration")
                .unwrap()
                .is_some()
        );

        crate::remover::Remover::new(&database, &logger, PromptHandler::new(true, false, false))
            .remove("fd-limit-integration")
            .unwrap();
        assert!(
            database
                .is_installed("fd-limit-integration")
                .unwrap()
                .is_none()
        );
        assert!(!root.path().join("usr/share/fd-limit").exists());
    }
}
