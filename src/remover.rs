use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use rustix::fd::OwnedFd;
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, fstat, open, openat, renameat, statat, unlinkat,
};

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

        let root = RemovalRoot::new(self.db.root())?;
        let preserved_files = resolve_preserved_files(&root, &arkinfo)?;

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
        let mut removed_files = Vec::new();

        for file_rel in &arkinfo.files {
            // Reuse the preserved file's handle so staging and restoration cannot
            // be redirected to different directories by an ancestor replacement.
            let file = match preserved_files
                .iter()
                .find(|(original, _)| original == file_rel)
            {
                Some((_, change)) => change.resolve_file(&root),
                None => root.resolve_file(file_rel),
            };
            let staged = match file {
                Ok(file) => backup_installed_file(file),
                Err(ArkError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            };
            match staged {
                Ok(Some(change)) => {
                    if let Some(parent) = file_rel.parent() {
                        affected_dirs.insert(parent.to_path_buf());
                    }
                    self.logger
                        .action("REMOVE", &format!("{}", file_rel.display()));
                    removed_files.push(change);
                }
                Ok(None) => {}
                Err(error) => {
                    restore_removed_files(&root, &removed_files)?;
                    return Err(error);
                }
            }
        }

        let mut restored_preserved_files = Vec::new();
        for (original, change) in &preserved_files {
            let file = match change.resolve_file(&root) {
                Ok(file) => file,
                Err(error) => {
                    rollback_files(&root, &restored_preserved_files, &removed_files)?;
                    return Err(error);
                }
            };
            match file.metadata() {
                Ok(_) => {
                    rollback_files(&root, &restored_preserved_files, &removed_files)?;
                    return Err(ArkError::PackageConflict(format!(
                        "Cannot restore pre-install file '{}' because the path is occupied",
                        original.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    rollback_files(&root, &restored_preserved_files, &removed_files)?;
                    return Err(ArkError::Io(error));
                }
            }
            if let Err(error) = change.restore(&root) {
                rollback_files(&root, &restored_preserved_files, &removed_files)?;
                return Err(error);
            }
            restored_preserved_files.push(change.clone());
        }

        // Remove database records only after all package files have been staged for deletion.
        self.logger.info("Updating package.db");
        if let Err(error) = self.db.remove_installed_package(pkg_name) {
            rollback_files(&root, &restored_preserved_files, &removed_files)?;
            return Err(error);
        }

        self.logger.info("Updating file.db");
        if let Err(error) = self.db.remove_file_ownership(&arkinfo.files) {
            self.restore_database_and_files(
                &arkinfo,
                &root,
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
                &arkinfo,
                &root,
                &removed_files,
                &restored_preserved_files,
                true,
                true,
            )?;
            return Err(error);
        }

        for change in &removed_files {
            let _ = change.delete_backup(&root);
        }

        // unlinkat checks emptiness atomically and never follows the final entry.
        for dir in affected_dirs {
            let mut current = dir;
            while current != Path::new("/") && !protected_dirs.contains(&current) {
                let result = root
                    .resolve_file(&current)
                    .and_then(|entry| entry.remove_dir());
                if result.is_err() {
                    break;
                }
                self.logger
                    .action("REMOVE DIR", &format!("{}", current.display()));
                if !current.pop() {
                    break;
                }
            }
        }

        crate::linker_cache::refresh_if_needed(self.db.root(), &arkinfo.files, self.logger);

        // 9 & 10. Log transaction and report success
        self.logger.info("DONE");
        Ok(())
    }

    fn restore_database_and_files(
        &self,
        arkinfo: &crate::database::ArkInfo,
        root: &RemovalRoot,
        removed_files: &[FileChange],
        restored_preserved_files: &[FileChange],
        restore_package: bool,
        restore_ownership: bool,
    ) -> Result<()> {
        if restore_ownership {
            self.db
                .add_file_ownership(&arkinfo.files, &arkinfo.name)
                .map_err(|error| {
                    ArkError::Generic(format!("Failed to restore file ownership: {error}"))
                })?;
        }
        if restore_package {
            self.db
                .add_installed_package(&arkinfo.name, &arkinfo.version)
                .map_err(|error| {
                    ArkError::Generic(format!("Failed to restore package record: {error}"))
                })?;
        }
        rollback_files(root, restored_preserved_files, removed_files)
    }
}

// Keep only the root and most recently resolved parent descriptors open. File
// changes retain a relative path and directory identity, then reopen through
// the pinned root when rollback or cleanup needs the parent again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

fn directory_identity(dir: &OwnedFd) -> Result<DirectoryIdentity> {
    let metadata = fstat(dir).map_err(io::Error::from)?;
    Ok(DirectoryIdentity {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
    })
}

struct RemovalRoot {
    path: PathBuf,
    canonical_path: PathBuf,
    dir: Rc<OwnedFd>,
    directory: RefCell<Option<(PathBuf, Rc<OwnedFd>, DirectoryIdentity)>>,
}

impl RemovalRoot {
    fn new(path: &Path) -> Result<Self> {
        let dir = Rc::new(
            open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        Ok(Self {
            path: path.to_path_buf(),
            canonical_path: path.canonicalize()?,
            dir,
            directory: RefCell::new(None),
        })
    }

    fn resolve_file(&self, path: &Path) -> Result<ResolvedFile> {
        let relative = path.strip_prefix("/").map_err(|_| {
            ArkError::InvalidPackage(format!("Invalid removal path '{}'", path.display()))
        })?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ArkError::InvalidPackage(format!(
                "Unsafe removal path '{}'",
                path.display()
            )));
        }
        let parent = self.path.join(relative).parent().unwrap().canonicalize()?;
        let relative_parent = parent.strip_prefix(&self.canonical_path).map_err(|_| {
            ArkError::InvalidPackage(format!(
                "Removal path '{}' resolves outside target root",
                path.display()
            ))
        })?;
        let (parent, parent_identity) = self.open_dir(relative_parent)?;
        Ok(ResolvedFile {
            parent,
            parent_path: relative_parent.to_path_buf(),
            parent_identity,
            name: relative.file_name().unwrap().to_os_string(),
        })
    }

    fn open_dir(&self, relative: &Path) -> Result<(Rc<OwnedFd>, DirectoryIdentity)> {
        if let Some((cached_path, cached_dir, cached_identity)) = self.directory.borrow().as_ref()
            && cached_path == relative
        {
            return Ok((Rc::clone(cached_dir), *cached_identity));
        }

        let mut dir = Rc::clone(&self.dir);
        // Canonicalization preserves support for in-root directory symlinks.
        // Reopen each canonical component without following links: a concurrent
        // replacement can fail resolution, but cannot escape the pinned root.
        for part in relative.components() {
            let Component::Normal(name) = part else {
                return Err(ArkError::InvalidPackage("Unsafe removal parent".into()));
            };
            dir = Rc::new(
                openat(
                    &*dir,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io::Error::from)?,
            );
        }
        let identity = directory_identity(&dir)?;
        *self.directory.borrow_mut() = Some((relative.to_path_buf(), Rc::clone(&dir), identity));
        Ok((dir, identity))
    }

    fn open_dir_matching(
        &self,
        relative: &Path,
        expected: DirectoryIdentity,
    ) -> Result<Rc<OwnedFd>> {
        let (dir, actual) = self.open_dir(relative)?;
        if actual != expected {
            return Err(ArkError::InvalidPackage(
                "Removal parent changed during transaction".into(),
            ));
        }
        Ok(dir)
    }
}

#[derive(Clone)]
struct ResolvedFile {
    parent: Rc<OwnedFd>,
    parent_path: PathBuf,
    parent_identity: DirectoryIdentity,
    name: OsString,
}

impl ResolvedFile {
    fn metadata(&self) -> io::Result<FileType> {
        let metadata = statat(&*self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)?;
        Ok(FileType::from_raw_mode(metadata.st_mode))
    }

    fn remove_dir(&self) -> Result<()> {
        unlinkat(&*self.parent, &self.name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        Ok(())
    }

    fn rename(&self, from: &OsString, to: &OsString) -> Result<()> {
        renameat(&*self.parent, from, &*self.parent, to).map_err(io::Error::from)?;
        Ok(())
    }
}

#[derive(Clone)]
struct FileChange {
    parent_path: PathBuf,
    parent_identity: DirectoryIdentity,
    name: OsString,
    backup: OsString,
}

impl FileChange {
    fn resolve_file(&self, root: &RemovalRoot) -> Result<ResolvedFile> {
        Ok(ResolvedFile {
            parent: root.open_dir_matching(&self.parent_path, self.parent_identity)?,
            parent_path: self.parent_path.clone(),
            parent_identity: self.parent_identity,
            name: self.name.clone(),
        })
    }

    fn restore(&self, root: &RemovalRoot) -> Result<()> {
        self.resolve_file(root)?.rename(&self.backup, &self.name)
    }

    fn delete_backup(&self, root: &RemovalRoot) -> Result<()> {
        let file = self.resolve_file(root)?;
        unlinkat(&*file.parent, &self.backup, AtFlags::empty()).map_err(io::Error::from)?;
        Ok(())
    }

    fn move_current_to_backup(&self, root: &RemovalRoot) -> Result<()> {
        self.resolve_file(root)?.rename(&self.name, &self.backup)
    }
}

fn resolve_preserved_files(
    root: &RemovalRoot,
    arkinfo: &crate::database::ArkInfo,
) -> Result<Vec<(PathBuf, FileChange)>> {
    let mut resolved = Vec::with_capacity(arkinfo.preserved_files.len());
    for (original, backup) in &arkinfo.preserved_files {
        if backup.parent() != original.parent() || backup.file_name().is_none() {
            return Err(ArkError::InvalidPackage(format!(
                "Invalid preserved backup '{}'",
                backup.display()
            )));
        }
        let file = root.resolve_file(original)?;
        let backup_name = backup.file_name().unwrap().to_os_string();
        let metadata = statat(&*file.parent, &backup_name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(io::Error::from)?;
        let kind = FileType::from_raw_mode(metadata.st_mode);
        if !(kind.is_file() || kind.is_symlink()) {
            return Err(ArkError::PackageConflict(format!(
                "Preserved backup '{}' is not a file",
                backup.display()
            )));
        }
        resolved.push((
            original.clone(),
            FileChange {
                parent_path: file.parent_path,
                parent_identity: file.parent_identity,
                name: file.name,
                backup: backup_name,
            },
        ));
    }
    Ok(resolved)
}

fn rollback_files(
    root: &RemovalRoot,
    restored_preserved_files: &[FileChange],
    removed_files: &[FileChange],
) -> Result<()> {
    for change in restored_preserved_files.iter().rev() {
        change.move_current_to_backup(root)?;
    }
    restore_removed_files(root, removed_files)
}

fn backup_installed_file(file: ResolvedFile) -> Result<Option<FileChange>> {
    let kind = match file.metadata() {
        Ok(kind) => kind,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ArkError::Io(error)),
    };
    if !(kind.is_file() || kind.is_symlink()) {
        return Err(ArkError::PackageConflict(format!(
            "Refusing to remove non-file package path '{}'",
            file.name.to_string_lossy()
        )));
    }
    // Use tempfile only for collision-resistant name generation and retrying
    // O_EXCL creation. Its pathname cleanup is disabled; cleanup uses unlinkat.
    let temporary = tempfile::Builder::new()
        .prefix(".arkpkg-remove-")
        .disable_cleanup(true)
        .make_in(".", |path| {
            openat(
                &*file.parent,
                path.file_name().unwrap(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)
        })?;
    let backup = temporary.path().file_name().unwrap().to_os_string();
    if let Err(error) =
        renameat(&*file.parent, &file.name, &*file.parent, &backup).map_err(io::Error::from)
    {
        let _ = unlinkat(&*file.parent, &backup, AtFlags::empty());
        return Err(ArkError::Io(error));
    }
    Ok(Some(FileChange {
        parent_path: file.parent_path,
        parent_identity: file.parent_identity,
        name: file.name,
        backup,
    }))
}

fn restore_removed_files(root: &RemovalRoot, removed_files: &[FileChange]) -> Result<()> {
    for change in removed_files.iter().rev() {
        change.restore(root)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[cfg(target_os = "linux")]
    fn open_parent_descriptors(base: &Path) -> usize {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| path.starts_with(base))
            .count()
    }

    #[test]
    fn staging_and_cleanup_stay_in_the_opened_parent_after_ancestor_replacement() {
        let target = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join("usr/bin")).unwrap();
        fs::create_dir(outside.path().join("bin")).unwrap();
        fs::write(target.path().join("usr/bin/tool"), b"package").unwrap();
        fs::write(outside.path().join("bin/tool"), b"outside").unwrap();
        let root = RemovalRoot::new(target.path()).unwrap();
        let file = root.resolve_file(Path::new("/usr/bin/tool")).unwrap();

        // Replace the ancestor at the former check/use boundary.
        fs::rename(target.path().join("usr"), target.path().join("moved")).unwrap();
        symlink(outside.path(), target.path().join("usr")).unwrap();
        let change = backup_installed_file(file).unwrap().unwrap();
        assert_eq!(
            fs::read(outside.path().join("bin/tool")).unwrap(),
            b"outside"
        );
        assert_eq!(fs::read_dir(outside.path().join("bin")).unwrap().count(), 1);
        assert_eq!(
            fs::read(target.path().join("moved/bin").join(&change.backup)).unwrap(),
            b"package"
        );

        // A pathname-based cleanup would delete this external sentinel.
        let sentinel = outside.path().join("bin").join(&change.backup);
        fs::write(&sentinel, b"external backup").unwrap();
        change.delete_backup(&root).unwrap();
        assert_eq!(fs::read(sentinel).unwrap(), b"external backup");
        assert_eq!(
            fs::read_dir(target.path().join("moved/bin"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn preserved_restoration_and_rollback_retain_the_verified_parent() {
        let target = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join("usr/bin")).unwrap();
        fs::create_dir(outside.path().join("bin")).unwrap();
        fs::write(target.path().join("usr/bin/tool"), b"package").unwrap();
        fs::write(target.path().join("usr/bin/.original"), b"base").unwrap();
        let info = crate::database::ArkInfo::parse(
            "Name: test\nVersion: 1.0.0\nFiles:\n/usr/bin/tool\nPreservedFiles:\n/usr/bin/tool=/usr/bin/.original\n"
        ).unwrap();
        let root = RemovalRoot::new(target.path()).unwrap();
        let preserved = resolve_preserved_files(&root, &info).unwrap();
        let original = &preserved[0].1;
        let original_file = original.resolve_file(&root).unwrap();
        let removed = backup_installed_file(original_file).unwrap().unwrap();

        fs::rename(target.path().join("usr"), target.path().join("moved")).unwrap();
        symlink(outside.path(), target.path().join("usr")).unwrap();
        for name in [&original.name, &original.backup, &removed.backup] {
            fs::write(outside.path().join("bin").join(name), b"outside").unwrap();
        }
        original.restore(&root).unwrap();
        assert_eq!(
            fs::read(target.path().join("moved/bin/tool")).unwrap(),
            b"base"
        );
        rollback_files(&root, std::slice::from_ref(original), &[removed]).unwrap();
        assert_eq!(
            fs::read(target.path().join("moved/bin/tool")).unwrap(),
            b"package"
        );
        assert_eq!(
            fs::read(target.path().join("moved/bin/.original")).unwrap(),
            b"base"
        );
        for entry in fs::read_dir(outside.path().join("bin")).unwrap() {
            assert_eq!(fs::read(entry.unwrap().path()).unwrap(), b"outside");
        }
    }

    #[test]
    fn parent_resolution_rejects_links_inserted_after_canonicalization() {
        for ancestor in ["usr", "usr/bin"] {
            let target = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            fs::create_dir_all(target.path().join("usr/bin")).unwrap();
            let root = RemovalRoot::new(target.path()).unwrap();
            let checked = target.path().join("usr/bin").canonicalize().unwrap();
            let relative = checked.strip_prefix(&root.canonical_path).unwrap();
            fs::rename(target.path().join(ancestor), target.path().join("moved")).unwrap();
            symlink(outside.path(), target.path().join(ancestor)).unwrap();
            assert!(root.open_dir(relative).is_err());
            assert!(root.resolve_file(Path::new("/usr/bin/tool")).is_err());
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn large_file_lists_reuse_the_open_parent_directory() {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join("usr/share/nscde")).unwrap();
        let root = RemovalRoot::new(target.path()).unwrap();
        let first = root
            .resolve_file(Path::new("/usr/share/nscde/file-0"))
            .unwrap();

        for index in 1..10_000 {
            let path = format!("/usr/share/nscde/file-{index}");
            let file = root.resolve_file(Path::new(&path)).unwrap();
            assert!(Rc::ptr_eq(&first.parent, &file.parent));
        }
    }

    #[test]
    fn removal_keeps_file_descriptors_bounded_across_many_parent_directories() {
        const DIRECTORY_COUNT: usize = 1_200;
        let target = TempDir::new().unwrap();
        let base = target.path().join("usr/share/fd-limit");
        fs::create_dir_all(&base).unwrap();
        let root = RemovalRoot::new(target.path()).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(open_parent_descriptors(&base), 0);
        let mut changes = Vec::with_capacity(DIRECTORY_COUNT);

        for index in 0..DIRECTORY_COUNT {
            let directory = base.join(format!("d{index:04}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("payload"), b"package").unwrap();
            let relative = format!("/usr/share/fd-limit/d{index:04}/payload");
            let file = root.resolve_file(Path::new(&relative)).unwrap();
            changes.push(backup_installed_file(file).unwrap().unwrap());
        }

        assert_eq!(changes.len(), DIRECTORY_COUNT);
        #[cfg(target_os = "linux")]
        assert!(
            open_parent_descriptors(&base) <= 1,
            "staging many removals must retain at most one parent descriptor"
        );
        for change in &changes {
            change.delete_backup(&root).unwrap();
        }
        assert!(fs::read_dir(base).unwrap().all(|entry| {
            fs::read_dir(entry.unwrap().path())
                .unwrap()
                .next()
                .is_none()
        }));
    }

    #[test]
    fn directory_cleanup_stays_in_the_opened_parent() {
        let target = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join("usr/share/empty")).unwrap();
        fs::create_dir_all(outside.path().join("share/empty")).unwrap();
        let root = RemovalRoot::new(target.path()).unwrap();
        let directory = root.resolve_file(Path::new("/usr/share/empty")).unwrap();
        fs::rename(target.path().join("usr"), target.path().join("moved")).unwrap();
        symlink(outside.path(), target.path().join("usr")).unwrap();
        directory.remove_dir().unwrap();
        assert!(!target.path().join("moved/share/empty").exists());
        assert!(outside.path().join("share/empty").is_dir());
        assert!(root.resolve_file(Path::new("/usr/share/empty")).is_err());
    }

    #[test]
    fn removal_supports_in_root_parent_links_and_unlinks_final_symlinks() {
        let target = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join("usr/bin")).unwrap();
        fs::write(outside.path().join("tool"), b"outside").unwrap();
        symlink("usr/bin", target.path().join("bin")).unwrap();
        symlink(
            target.path().join("usr/bin"),
            target.path().join("absolute-bin"),
        )
        .unwrap();
        let root = RemovalRoot::new(target.path()).unwrap();
        for parent in ["bin", "absolute-bin"] {
            symlink(
                outside.path().join("tool"),
                target.path().join("usr/bin/tool"),
            )
            .unwrap();
            let file = root
                .resolve_file(&Path::new("/").join(parent).join("tool"))
                .unwrap();
            let change = backup_installed_file(file).unwrap().unwrap();
            change.delete_backup(&root).unwrap();
            assert!(fs::symlink_metadata(target.path().join("usr/bin/tool")).is_err());
            assert_eq!(fs::read(outside.path().join("tool")).unwrap(), b"outside");
        }
    }
}
