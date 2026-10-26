use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use crate::errors::{ArkError, Result};
use crate::metadata::{ProvideItem, validate_package_atom, validate_package_name};
use crate::version::{DependencyReq, Version};

/// Installed package metadata record (`.arkinfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArkInfo {
    pub name: String,
    pub version: Version,
    pub arch: String,
    pub installed_at: String,
    pub checksum: String,
    pub files: Vec<PathBuf>,
    /// Per-file SHA-256 values, recorded as (installed path, digest).
    pub file_hashes: Vec<(PathBuf, String)>,
    pub dependencies: Vec<DependencyReq>,
    pub provides: Vec<ProvideItem>,
    pub conflicts: Vec<String>,
    /// Files that existed before installation, recorded as (installed path, backup path).
    pub preserved_files: Vec<(PathBuf, PathBuf)>,
}

impl ArkInfo {
    /// Parses an `.arkinfo` file string.
    pub fn parse(content: &str) -> Result<Self> {
        let mut name = None;
        let mut version = None;
        let mut arch = None;
        let mut installed_at = None;
        let mut checksum = None;
        let mut files = Vec::new();
        let mut file_hashes = Vec::new();
        let mut dependencies = Vec::new();
        let mut provides = Vec::new();
        let mut conflicts = Vec::new();
        let mut preserved_files = Vec::new();

        let mut current_section: Option<&str> = None;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if let Some((key, val)) = trimmed.split_once(':') {
                let field = key.trim();
                let v = val.trim();

                match field {
                    "Name" => {
                        name = Some(v.to_string());
                        current_section = None;
                    }
                    "Version" => {
                        version = Some(Version::parse(v)?);
                        current_section = None;
                    }
                    "Arch" => {
                        arch = Some(v.to_string());
                        current_section = None;
                    }
                    "InstalledAt" => {
                        installed_at = Some(v.to_string());
                        current_section = None;
                    }
                    "Checksum" => {
                        checksum = Some(v.to_string());
                        current_section = None;
                    }
                    "Files" => {
                        current_section = Some("Files");
                        if !v.is_empty() {
                            files.push(PathBuf::from(v));
                        }
                    }
                    "FileHashes" => {
                        current_section = Some("FileHashes");
                        if !v.is_empty() {
                            file_hashes.push(parse_file_hash(v)?);
                        }
                    }
                    "Dependencies" => {
                        current_section = Some("Dependencies");
                        if !v.is_empty() {
                            dependencies.push(DependencyReq::parse(v)?);
                        }
                    }
                    "Provides" => {
                        current_section = Some("Provides");
                        if !v.is_empty() {
                            provides.push(ProvideItem::parse(v)?);
                        }
                    }
                    "Conflicts" => {
                        current_section = Some("Conflicts");
                        if !v.is_empty() {
                            conflicts.push(v.to_string());
                        }
                    }
                    "PreservedFiles" => {
                        current_section = Some("PreservedFiles");
                        if !v.is_empty() {
                            preserved_files.push(parse_preserved_file(v)?);
                        }
                    }
                    _ => {
                        if let Some(sec) = current_section {
                            match sec {
                                "Files" => files.push(PathBuf::from(trimmed)),
                                "FileHashes" => file_hashes.push(parse_file_hash(trimmed)?),
                                "Dependencies" => dependencies.push(DependencyReq::parse(trimmed)?),
                                "Provides" => provides.push(ProvideItem::parse(trimmed)?),
                                "Conflicts" => conflicts.push(trimmed.to_string()),
                                "PreservedFiles" => {
                                    preserved_files.push(parse_preserved_file(trimmed)?)
                                }
                                _ => {}
                            }
                        }
                    }
                }
            } else if let Some(sec) = current_section {
                match sec {
                    "Files" => files.push(PathBuf::from(trimmed)),
                    "FileHashes" => file_hashes.push(parse_file_hash(trimmed)?),
                    "Dependencies" => dependencies.push(DependencyReq::parse(trimmed)?),
                    "Provides" => provides.push(ProvideItem::parse(trimmed)?),
                    "Conflicts" => conflicts.push(trimmed.to_string()),
                    "PreservedFiles" => preserved_files.push(parse_preserved_file(trimmed)?),
                    _ => {}
                }
            }
        }

        let name = name.ok_or_else(|| ArkError::InvalidPackage("ArkInfo missing Name".into()))?;
        validate_package_name(&name)?;
        let version =
            version.ok_or_else(|| ArkError::InvalidPackage("ArkInfo missing Version".into()))?;
        let arch = arch.unwrap_or_else(|| "x86_64".into());
        validate_package_atom(&arch, "architecture")?;
        for path in &files {
            validate_installed_path(path)?;
        }
        validate_file_hashes(&files, &file_hashes)?;
        validate_preserved_files(&files, &preserved_files)?;
        for dependency in &dependencies {
            validate_package_name(&dependency.name)?;
        }
        for conflict in &conflicts {
            validate_package_name(conflict)?;
        }
        let installed_at = installed_at.unwrap_or_default();
        let checksum = checksum.unwrap_or_default();

        Ok(Self {
            name,
            version,
            arch,
            installed_at,
            checksum,
            files,
            file_hashes,
            dependencies,
            provides,
            conflicts,
            preserved_files,
        })
    }

    /// Serializes `.arkinfo` to plain text format.
    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Name: {}\n", self.name));
        out.push_str(&format!("Version: {}\n", self.version));
        out.push_str(&format!("Arch: {}\n", self.arch));
        out.push_str(&format!("InstalledAt: {}\n", self.installed_at));
        out.push_str(&format!("Checksum: {}\n", self.checksum));

        out.push_str("Files:\n");
        for f in &self.files {
            out.push_str(&format!("{}\n", f.display()));
        }

        if !self.file_hashes.is_empty() {
            out.push_str("FileHashes:\n");
            for (path, hash) in &self.file_hashes {
                out.push_str(&format!("{}:{}\n", path.display(), hash));
            }
        }

        if !self.preserved_files.is_empty() {
            out.push_str("PreservedFiles:\n");
            for (original, backup) in &self.preserved_files {
                out.push_str(&format!("{}={}\n", original.display(), backup.display()));
            }
        }

        if !self.dependencies.is_empty() {
            out.push_str("Dependencies:\n");
            for dependency in &self.dependencies {
                out.push_str(&format!("{}\n", dependency));
            }
        }
        if !self.provides.is_empty() {
            out.push_str("Provides:\n");
            for p in &self.provides {
                out.push_str(&format!("{}\n", p));
            }
        }
        if !self.conflicts.is_empty() {
            out.push_str("Conflicts:\n");
            for conflict in &self.conflicts {
                out.push_str(&format!("{}\n", conflict));
            }
        }

        out
    }
}

fn parse_file_hash(value: &str) -> Result<(PathBuf, String)> {
    let (path, hash) = value
        .rsplit_once(':')
        .ok_or_else(|| ArkError::InvalidPackage("Malformed FileHashes entry".into()))?;
    Ok((PathBuf::from(path), hash.to_string()))
}

fn validate_file_hashes(files: &[PathBuf], file_hashes: &[(PathBuf, String)]) -> Result<()> {
    if file_hashes.is_empty() {
        return Ok(());
    }
    if file_hashes.len() != files.len() {
        return Err(ArkError::InvalidPackage(
            "FileHashes must contain one digest for every installed file".into(),
        ));
    }
    let file_paths: HashSet<_> = files.iter().cloned().collect();
    let mut seen = HashSet::new();
    for (path, hash) in file_hashes {
        validate_installed_path(path)?;
        if !file_paths.contains(path) || !seen.insert(path.clone()) {
            return Err(ArkError::InvalidPackage(format!(
                "Invalid or duplicate file hash path '{}'",
                path.display()
            )));
        }
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ArkError::InvalidPackage(format!(
                "Invalid SHA-256 digest for '{}'",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_installed_path(path: &Path) -> Result<()> {
    let value = path.to_str().ok_or_else(|| {
        ArkError::InvalidPackage("Installed file paths must be valid UTF-8".into())
    })?;
    if !value.starts_with('/')
        || value == "/"
        || value.contains("//")
        || value.contains('\0')
        || value.contains('=')
        || value.ends_with('/')
        || value.bytes().any(|byte| byte.is_ascii_control())
        || value
            .split('/')
            .any(|component| component == "." || component == "..")
    {
        return Err(ArkError::InvalidPackage(format!(
            "Unsafe installed file path '{}'",
            path.display()
        )));
    }
    Ok(())
}

fn parse_preserved_file(value: &str) -> Result<(PathBuf, PathBuf)> {
    let (original, backup) = value
        .split_once('=')
        .ok_or_else(|| ArkError::InvalidPackage("Malformed PreservedFiles entry".into()))?;
    Ok((PathBuf::from(original), PathBuf::from(backup)))
}

fn validate_preserved_files(
    files: &[PathBuf],
    preserved_files: &[(PathBuf, PathBuf)],
) -> Result<()> {
    let mut originals = HashSet::new();
    let mut backups = HashSet::new();
    for (original, backup) in preserved_files {
        validate_installed_path(original)?;
        validate_installed_path(backup)?;
        if !files.contains(original) {
            return Err(ArkError::InvalidPackage(format!(
                "Preserved path '{}' is not owned by this package",
                original.display()
            )));
        }
        if original == backup || original.parent() != backup.parent() {
            return Err(ArkError::InvalidPackage(format!(
                "Backup '{}' must be a sibling of preserved path '{}'",
                backup.display(),
                original.display()
            )));
        }
        if files.contains(backup) || !originals.insert(original) || !backups.insert(backup) {
            return Err(ArkError::InvalidPackage(format!(
                "Duplicate or conflicting preserved path '{}'",
                original.display()
            )));
        }
    }
    Ok(())
}

// Only single, fixed path components are passed to this helper.
fn open_state_directory(parent: &impl AsFd, name: &str) -> io::Result<OwnedFd> {
    match mkdirat(parent, name, Mode::from_raw_mode(0o777)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Reject payload files at database paths, including their ancestors and descendants.
pub(crate) fn validate_payload_path(relative: &Path) -> Result<()> {
    for reserved in [
        "etc/arkpkg/package.db",
        "etc/arkpkg/file.db",
        "etc/arkpkg/packages",
    ] {
        let reserved = Path::new(reserved);
        if relative.starts_with(reserved) || reserved.starts_with(relative) {
            return Err(ArkError::InvalidPackage(format!(
                "Package file '{}' overlaps reserved database state",
                relative.display()
            )));
        }
    }
    Ok(())
}

/// Package and file database manager operating under a root directory.
#[derive(Debug, Clone)]
pub struct Database {
    root: PathBuf,
}

impl Database {
    /// Creates a Database instance operating at the given root directory.
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn package_db_path(&self) -> PathBuf {
        self.root.join("etc/arkpkg/package.db")
    }

    fn file_db_path(&self) -> PathBuf {
        self.root.join("etc/arkpkg/file.db")
    }

    fn packages_dir(&self) -> PathBuf {
        self.root.join("etc/arkpkg/packages")
    }

    fn arkinfo_path(&self, pkg_name: &str) -> Result<PathBuf> {
        validate_package_name(pkg_name)?;
        Ok(self.packages_dir().join(format!("{}.arkinfo", pkg_name)))
    }

    /// Ensures state directories and regular database files exist without following symlinks.
    pub fn init(&self) -> Result<()> {
        // The caller selects the root; every component beneath it is opened relative
        // to a verified directory handle so a symlink cannot redirect creation.
        fs::create_dir_all(&self.root)?;
        let root = open(
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let etc = open_state_directory(&root, "etc")?;
        let db_dir = open_state_directory(&etc, "arkpkg")?;
        open_state_directory(&db_dir, "packages")?;
        let var = open_state_directory(&root, "var")?;
        open_state_directory(&var, "log")?;

        for name in ["package.db", "file.db"] {
            // No truncation, even if another initializer creates the file first.
            // NONBLOCK lets us reject a FIFO without waiting for a writer.
            let file = File::from(
                openat(
                    &db_dir,
                    name,
                    OFlags::RDONLY
                        | OFlags::CREATE
                        | OFlags::NOFOLLOW
                        | OFlags::NONBLOCK
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o666),
                )
                .map_err(io::Error::from)?,
            );
            if !file.metadata()?.is_file() {
                return Err(ArkError::InvalidPackage(format!(
                    "Database '{name}' is not a regular file"
                )));
            }
        }
        Ok(())
    }

    /// Reads all installed packages from `package.db`.
    pub fn read_installed_packages(&self) -> Result<Vec<(String, Version)>> {
        self.init()?;
        let file = File::open(self.package_db_path())?;
        let reader = BufReader::new(file);
        let mut list = Vec::new();

        for line in reader.lines() {
            let l = line?;
            let trimmed = l.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut fields = trimmed.split_whitespace();
            let name = fields.next().ok_or_else(|| {
                ArkError::InvalidPackage("Malformed package database entry".into())
            })?;
            let version = fields.next().ok_or_else(|| {
                ArkError::InvalidPackage("Malformed package database entry".into())
            })?;
            if fields.next().is_some() {
                return Err(ArkError::InvalidPackage(
                    "Malformed package database entry".into(),
                ));
            }
            validate_package_name(name)?;
            list.push((name.to_string(), Version::parse(version)?));
        }
        Ok(list)
    }

    /// Checks if a package is installed, returning its Version if present.
    pub fn is_installed(&self, pkg_name: &str) -> Result<Option<Version>> {
        validate_package_name(pkg_name)?;
        let pkgs = self.read_installed_packages()?;
        for (name, ver) in pkgs {
            if name.eq_ignore_ascii_case(pkg_name) {
                return Ok(Some(ver));
            }
        }
        Ok(None)
    }

    /// Atomically updates `package.db`.
    pub fn add_installed_package(&self, pkg_name: &str, version: &Version) -> Result<()> {
        validate_package_name(pkg_name)?;
        let mut pkgs = self.read_installed_packages()?;
        pkgs.retain(|(n, _)| !n.eq_ignore_ascii_case(pkg_name));
        pkgs.push((pkg_name.to_string(), version.clone()));

        self.atomic_write_package_db(&pkgs)
    }

    /// Atomically removes a package entry from `package.db`.
    pub fn remove_installed_package(&self, pkg_name: &str) -> Result<()> {
        validate_package_name(pkg_name)?;
        let mut pkgs = self.read_installed_packages()?;
        pkgs.retain(|(n, _)| !n.eq_ignore_ascii_case(pkg_name));

        self.atomic_write_package_db(&pkgs)
    }

    fn atomic_write_package_db(&self, pkgs: &[(String, Version)]) -> Result<()> {
        let db_dir = self.root.join("etc/arkpkg");
        let mut temp_file = NamedTempFile::new_in(&db_dir)?;

        for (n, v) in pkgs {
            writeln!(temp_file, "{} {}", n, v)?;
        }
        temp_file.flush()?;
        temp_file.persist(self.package_db_path()).map_err(|error| {
            ArkError::Io(std::io::Error::other(format!(
                "Failed to persist package.db: {error}"
            )))
        })?;
        Ok(())
    }

    /// Reads all file ownership mappings from `file.db`.
    pub fn read_file_ownership(&self) -> Result<HashMap<PathBuf, String>> {
        self.init()?;
        let file = File::open(self.file_db_path())?;
        let reader = BufReader::new(file);
        let mut map = HashMap::new();

        for line in reader.lines() {
            let l = line?;
            let trimmed = l.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((path_str, pkg_name)) = trimmed.split_once('=') {
                let path = PathBuf::from(path_str.trim());
                let name = pkg_name.trim();
                validate_installed_path(&path)?;
                validate_package_name(name)?;
                map.insert(path, name.to_string());
            }
        }
        Ok(map)
    }

    /// Returns the owner package of a specific file path, if registered in `file.db`.
    pub fn get_file_owner<P: AsRef<Path>>(&self, path: P) -> Result<Option<String>> {
        validate_installed_path(path.as_ref())?;
        let map = self.read_file_ownership()?;
        Ok(map.get(path.as_ref()).cloned())
    }

    /// Atomically registers file ownerships for a package in `file.db`.
    pub fn add_file_ownership(&self, files: &[PathBuf], pkg_name: &str) -> Result<()> {
        let mut map = self.read_file_ownership()?;
        self.add_file_ownership_from_map(&mut map, files, pkg_name)
    }

    /// Registers ownerships using a map already loaded by the caller.
    pub(crate) fn add_file_ownership_from_map(
        &self,
        map: &mut HashMap<PathBuf, String>,
        files: &[PathBuf],
        pkg_name: &str,
    ) -> Result<()> {
        validate_package_name(pkg_name)?;
        for f in files {
            validate_installed_path(f)?;
        }

        let previous_owners: Vec<_> = files
            .iter()
            .map(|file| (file.clone(), map.get(file).cloned()))
            .collect();
        for f in files {
            map.insert(f.clone(), pkg_name.to_string());
        }

        if let Err(error) = self.atomic_write_file_db(map) {
            for (file, previous_owner) in previous_owners {
                if let Some(owner) = previous_owner {
                    map.insert(file, owner);
                } else {
                    map.remove(&file);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    /// Atomically removes file ownership entries for specific files from `file.db`.
    pub fn remove_file_ownership(&self, files: &[PathBuf]) -> Result<()> {
        let mut map = self.read_file_ownership()?;
        for f in files {
            validate_installed_path(f)?;
            map.remove(f);
        }

        self.atomic_write_file_db(&map)
    }

    fn atomic_write_file_db(&self, map: &HashMap<PathBuf, String>) -> Result<()> {
        let db_dir = self.root.join("etc/arkpkg");
        let mut temp_file = NamedTempFile::new_in(&db_dir)?;

        for (path, pkg) in map {
            writeln!(temp_file, "{}={}", path.display(), pkg)?;
        }
        temp_file.flush()?;
        temp_file.persist(self.file_db_path()).map_err(|error| {
            ArkError::Io(std::io::Error::other(format!(
                "Failed to persist file.db: {error}"
            )))
        })?;
        Ok(())
    }

    /// Atomically writes an `.arkinfo` file for an installed package.
    pub fn write_arkinfo(&self, arkinfo: &ArkInfo) -> Result<()> {
        self.init()?;
        validate_package_name(&arkinfo.name)?;
        validate_package_atom(&arkinfo.arch, "architecture")?;
        for path in &arkinfo.files {
            validate_installed_path(path)?;
        }
        let target_path = self.arkinfo_path(&arkinfo.name)?;
        let mut temp_file = NamedTempFile::new_in(self.packages_dir())?;
        temp_file.write_all(arkinfo.to_string_pretty().as_bytes())?;
        temp_file.flush()?;
        temp_file.persist(target_path).map_err(|error| {
            ArkError::Io(std::io::Error::other(format!(
                "Failed to persist arkinfo: {error}"
            )))
        })?;
        Ok(())
    }

    /// Reads `.arkinfo` for an installed package.
    pub fn read_arkinfo(&self, pkg_name: &str) -> Result<ArkInfo> {
        let path = self.arkinfo_path(pkg_name)?;
        if !path.exists() {
            return Err(ArkError::PackageNotFound(format!(
                "Package metadata info for '{}' not found",
                pkg_name
            )));
        }
        let content = fs::read_to_string(path)?;
        let arkinfo = ArkInfo::parse(&content)?;
        if !arkinfo.name.eq_ignore_ascii_case(pkg_name) {
            return Err(ArkError::InvalidPackage(format!(
                "Package metadata name '{}' does not match requested package '{}'",
                arkinfo.name, pkg_name
            )));
        }
        Ok(arkinfo)
    }

    /// Deletes the `.arkinfo` file for a package.
    pub fn remove_arkinfo(&self, pkg_name: &str) -> Result<()> {
        let path = self.arkinfo_path(pkg_name)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn init_rejects_database_symlinks_without_touching_the_target() {
        for name in ["package.db", "file.db"] {
            for existing in [false, true] {
                let root = TempDir::new().unwrap();
                let outside = TempDir::new().unwrap();
                let target = outside.path().join("target");
                if existing {
                    fs::write(&target, b"keep me").unwrap();
                }
                let db_dir = root.path().join("etc/arkpkg");
                fs::create_dir_all(&db_dir).unwrap();
                std::os::unix::fs::symlink(&target, db_dir.join(name)).unwrap();

                assert!(
                    Database::new(root.path()).init().is_err(),
                    "accepted {name}"
                );
                if existing {
                    assert_eq!(fs::read(&target).unwrap(), b"keep me");
                } else {
                    assert!(!target.exists());
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn init_rejects_symlinked_state_directories() {
        for relative in ["etc", "etc/arkpkg", "etc/arkpkg/packages", "var", "var/log"] {
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let link = root.path().join(relative);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();

            assert!(
                Database::new(root.path()).init().is_err(),
                "accepted {relative}"
            );
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn init_rejects_non_file_databases() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("etc/arkpkg/package.db")).unwrap();
        assert!(Database::new(root.path()).init().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn init_rejects_fifo_databases() {
        let root = TempDir::new().unwrap();
        let db_dir = root.path().join("etc/arkpkg");
        fs::create_dir_all(&db_dir).unwrap();
        let parent = File::open(&db_dir).unwrap();
        rustix::fs::mkfifoat(&parent, "package.db", Mode::from_raw_mode(0o600)).unwrap();
        assert!(Database::new(root.path()).init().is_err());
    }

    #[test]
    fn test_database_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::new(temp_dir.path());

        let ver = Version::parse("5.3.0").unwrap();
        db.add_installed_package("bash", &ver).unwrap();

        assert_eq!(db.is_installed("bash").unwrap(), Some(ver));

        let file_path = PathBuf::from("/usr/bin/bash");
        db.add_file_ownership(std::slice::from_ref(&file_path), "bash")
            .unwrap();

        assert_eq!(
            db.get_file_owner(&file_path).unwrap(),
            Some("bash".to_string())
        );

        let extra_file = PathBuf::from("/usr/share/bash/README");
        let mut ownership = db.read_file_ownership().unwrap();
        db.add_file_ownership_from_map(&mut ownership, std::slice::from_ref(&extra_file), "bash")
            .unwrap();
        assert_eq!(ownership.get(&extra_file).map(String::as_str), Some("bash"));
        assert_eq!(db.read_file_ownership().unwrap(), ownership);

        let arkinfo = ArkInfo {
            name: "bash".into(),
            version: Version::parse("5.3.0").unwrap(),
            arch: "x86_64".into(),
            installed_at: "2026-07-29T12:00:00Z".into(),
            checksum: "abc123hash".into(),
            files: vec![file_path.clone()],
            file_hashes: vec![(file_path.clone(), "a".repeat(64))],
            dependencies: vec![DependencyReq::parse("glibc>=2.39").unwrap()],
            provides: vec![],
            conflicts: vec!["busybox".into()],
            preserved_files: vec![(
                PathBuf::from("/usr/bin/bash"),
                PathBuf::from("/usr/bin/.tmp-original-bash"),
            )],
        };

        db.write_arkinfo(&arkinfo).unwrap();
        let loaded = db.read_arkinfo("bash").unwrap();
        assert_eq!(loaded.name, "bash");
        assert_eq!(loaded.files, vec![file_path]);
        assert_eq!(loaded.file_hashes, arkinfo.file_hashes);
        assert_eq!(loaded.dependencies, arkinfo.dependencies);
        assert_eq!(loaded.conflicts, arkinfo.conflicts);
        assert_eq!(loaded.preserved_files, arkinfo.preserved_files);
    }

    #[test]
    fn failed_file_ownership_persist_restores_the_supplied_map() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::new(temp_dir.path());
        db.init().unwrap();

        let existing = PathBuf::from("/usr/bin/existing");
        db.add_file_ownership(std::slice::from_ref(&existing), "original")
            .unwrap();
        let mut ownership = db.read_file_ownership().unwrap();
        let before = ownership.clone();

        fs::remove_file(db.file_db_path()).unwrap();
        fs::create_dir(db.file_db_path()).unwrap();

        let added = PathBuf::from("/usr/bin/added");
        assert!(
            db.add_file_ownership_from_map(&mut ownership, &[existing, added], "replacement")
                .is_err()
        );
        assert_eq!(ownership, before);
    }

    #[test]
    fn old_arkinfo_without_preserved_files_remains_readable() {
        let arkinfo =
            ArkInfo::parse("Name: sample\nVersion: 1.0.0\nFiles:\n/usr/bin/sample\n").unwrap();
        assert!(arkinfo.preserved_files.is_empty());
        assert!(arkinfo.file_hashes.is_empty());
    }

    #[test]
    fn rejects_incomplete_or_invalid_file_hashes() {
        let valid_path = "/usr/bin/sample";
        let invalid = [
            format!(
                "Name: sample\nVersion: 1.0\nFiles:\n{valid_path}\nFileHashes:\n{valid_path}:bad\n"
            ),
            format!(
                "Name: sample\nVersion: 1.0\nFiles:\n{valid_path}\nFileHashes:\n/usr/bin/other:{}\n",
                "a".repeat(64)
            ),
        ];
        for content in invalid {
            assert!(ArkInfo::parse(&content).is_err());
        }
    }

    #[test]
    fn rejects_unsafe_installed_paths_and_package_names() {
        for path in [
            "relative/file",
            "/../etc/passwd",
            "/usr//bin/tool",
            "/usr/bin/a=b",
        ] {
            let content = format!("Name: sample\nVersion: 1.0\nFiles:\n{path}\n");
            assert!(ArkInfo::parse(&content).is_err(), "accepted {path:?}");
        }

        let invalid_preserved = [
            "/usr/bin/other=/usr/bin/.tmp-backup",
            "/usr/bin/tool=/var/tmp/.tmp-backup",
            "/usr/bin/tool=/usr/bin/tool",
        ];
        for entry in invalid_preserved {
            let content = format!(
                "Name: sample\nVersion: 1.0\nFiles:\n/usr/bin/tool\nPreservedFiles:\n{entry}\n"
            );
            assert!(ArkInfo::parse(&content).is_err(), "accepted {entry:?}");
        }

        let root = TempDir::new().unwrap();
        let db = Database::new(root.path());
        assert!(db.read_arkinfo("../outside").is_err());
    }
}
