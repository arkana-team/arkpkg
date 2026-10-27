use cap_std::ambient_authority;
use cap_std::fs::Dir;
use cap_tempfile::TempFile;
use rustix::fs::{Mode, OFlags, openat};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::checksum::typed_file_digest;
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
    /// Per-file integrity values, recorded as (installed path, tagged digest).
    /// New records use `file-sha256=<hex>` or `symlink-sha256=<hex>`.
    /// Legacy bare SHA-256 values remain readable but cannot verify object type.
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
        let digest = typed_file_digest(hash).unwrap_or(hash);
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ArkError::InvalidPackage(format!(
                "Invalid SHA-256 digest for '{}'",
                path.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_installed_path(path: &Path) -> Result<()> {
    let value = path.to_str().ok_or_else(|| {
        ArkError::InvalidPackage("Installed file paths must be valid UTF-8".into())
    })?;
    if !value.starts_with('/')
        || value == "/"
        || value.contains("//")
        || value.contains('\0')
        || value.contains('=')
        || value.ends_with('/')
        // The line-oriented database readers trim surrounding whitespace.
        || value.trim() != value
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

/// Reject payload paths that cannot be recorded losslessly or overlap database state.
pub(crate) fn validate_payload_path(relative: &Path) -> Result<()> {
    validate_installed_path(&Path::new("/").join(relative))?;
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
    root_dir: Arc<OnceLock<Dir>>,
}

impl Database {
    /// Creates a Database instance operating at the given root directory.
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            root_dir: Arc::new(OnceLock::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // Only the user-selected root is opened with ambient authority. Keep its
    // handle across calls (and clones), so replacing its pathname cannot redirect IO.
    fn root_dir(&self) -> std::io::Result<&Dir> {
        if self.root_dir.get().is_none() {
            let dir = Dir::open_ambient_dir(&self.root, ambient_authority())?;
            let _ = self.root_dir.set(dir);
        }
        Ok(self.root_dir.get().expect("root directory initialized"))
    }

    fn packages_dir(&self) -> std::io::Result<Dir> {
        self.root_dir()?.open_dir("etc/arkpkg/packages")
    }

    fn arkinfo_name(pkg_name: &str) -> Result<String> {
        validate_package_name(pkg_name)?;
        Ok(format!("{}.arkinfo", pkg_name))
    }

    /// Ensures that database directories exist without resolving outside the root.
    pub fn init(&self) -> Result<()> {
        self.init_db_dir()?;
        Ok(())
    }

    fn init_db_dir(&self) -> Result<Dir> {
        if let Err(error) = self.root_dir() {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
            // Preserve support for a not-yet-created, user-selected root.
            fs::create_dir_all(&self.root)?;
        }
        let root = self.root_dir()?;
        root.create_dir_all("etc/arkpkg/packages")?;
        root.create_dir_all("var/log")?;
        let db_dir = root.open_dir("etc/arkpkg")?;
        for name in ["package.db", "file.db"] {
            Self::open_database_file(&db_dir, name, true)?;
        }
        Ok(db_dir)
    }

    fn open_database_file(dir: &Dir, name: &str, create: bool) -> Result<File> {
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let mode = if create {
            flags |= OFlags::CREATE;
            Mode::from_raw_mode(0o666)
        } else {
            Mode::empty()
        };
        let file = File::from(openat(dir, name, flags, mode).map_err(std::io::Error::from)?);
        if !file.metadata()?.is_file() {
            return Err(ArkError::InvalidPackage(format!(
                "Database '{name}' is not a regular file"
            )));
        }
        Ok(file)
    }

    fn temporary_file(dir: &Dir) -> Result<TempFile<'_>> {
        let file = TempFile::new(dir)?;
        // Match NamedTempFile's private permissions before writing any data.
        #[cfg(unix)]
        {
            use cap_std::fs::PermissionsExt;
            file.as_file()
                .set_permissions(cap_std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(file)
    }

    /// Reads all installed packages from `package.db`.
    pub fn read_installed_packages(&self) -> Result<Vec<(String, Version)>> {
        let db_dir = self.init_db_dir()?;
        let file = Self::open_database_file(&db_dir, "package.db", false)?;
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
        let db_dir = self.init_db_dir()?;
        let mut temp_file = Self::temporary_file(&db_dir)?;

        for (n, v) in pkgs {
            writeln!(temp_file, "{} {}", n, v)?;
        }
        temp_file.flush()?;
        temp_file.replace("package.db").map_err(|error| {
            ArkError::Io(std::io::Error::other(format!(
                "Failed to persist package.db: {error}"
            )))
        })?;
        Ok(())
    }

    /// Reads all file ownership mappings from `file.db`.
    pub fn read_file_ownership(&self) -> Result<HashMap<PathBuf, String>> {
        let db_dir = self.init_db_dir()?;
        let file = Self::open_database_file(&db_dir, "file.db", false)?;
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
        let db_dir = self.init_db_dir()?;
        let mut temp_file = Self::temporary_file(&db_dir)?;

        for (path, pkg) in map {
            writeln!(temp_file, "{}={}", path.display(), pkg)?;
        }
        temp_file.flush()?;
        temp_file.replace("file.db").map_err(|error| {
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
        validate_file_hashes(&arkinfo.files, &arkinfo.file_hashes)?;
        validate_preserved_files(&arkinfo.files, &arkinfo.preserved_files)?;
        let target_name = Self::arkinfo_name(&arkinfo.name)?;
        let packages_dir = self.packages_dir()?;
        let mut temp_file = Self::temporary_file(&packages_dir)?;
        temp_file.write_all(arkinfo.to_string_pretty().as_bytes())?;
        temp_file.flush()?;
        temp_file.replace(target_name).map_err(|error| {
            ArkError::Io(std::io::Error::other(format!(
                "Failed to persist arkinfo: {error}"
            )))
        })?;
        Ok(())
    }

    /// Reads `.arkinfo` for an installed package.
    pub fn read_arkinfo(&self, pkg_name: &str) -> Result<ArkInfo> {
        let name = Self::arkinfo_name(pkg_name)?;
        let content = self
            .packages_dir()
            .and_then(|dir| dir.read_to_string(name))
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    ArkError::PackageNotFound(format!(
                        "Package metadata info for '{}' not found",
                        pkg_name
                    ))
                } else {
                    error.into()
                }
            })?;
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
        let name = Self::arkinfo_name(pkg_name)?;
        match self.packages_dir().and_then(|dir| dir.remove_file(name)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn database_rejects_escaping_directory_symlinks() {
        use std::os::unix::fs::symlink;

        for link in ["etc", "etc/arkpkg", "etc/arkpkg/packages", "var", "var/log"] {
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let link_path = root.path().join(link);
            fs::create_dir_all(link_path.parent().unwrap()).unwrap();
            symlink(outside.path(), &link_path).unwrap();
            let db = Database::new(root.path());
            assert!(db.init().is_err(), "accepted {link}");
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_rejects_fifo_state_files_without_blocking() {
        let root = TempDir::new().unwrap();
        let db_dir = root.path().join("etc/arkpkg");
        fs::create_dir_all(&db_dir).unwrap();
        let directory = File::open(&db_dir).unwrap();
        rustix::fs::mkfifoat(&directory, "package.db", Mode::from_raw_mode(0o600)).unwrap();

        assert!(Database::new(root.path()).init().is_err());
    }

    #[test]
    fn package_payloads_cannot_overlap_database_state() {
        for path in [
            "etc",
            "etc/arkpkg",
            "etc/arkpkg/package.db",
            "etc/arkpkg/file.db",
            "etc/arkpkg/packages",
            "etc/arkpkg/packages/sample.arkinfo",
        ] {
            assert!(
                validate_payload_path(Path::new(path)).is_err(),
                "accepted {path}"
            );
        }
        assert!(validate_payload_path(Path::new("etc/arkpkg-extra/file")).is_ok());
        assert!(validate_payload_path(Path::new("etc/profile")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn database_operations_reject_replaced_ancestry() {
        use std::os::unix::fs::symlink;

        for link in ["etc", "etc/arkpkg", "etc/arkpkg/packages"] {
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let db = Database::new(root.path());
            db.init().unwrap();
            let info = ArkInfo::parse("Name: sample\nVersion: 1.0\n").unwrap();
            let external_db = Database::new(outside.path());
            external_db
                .add_installed_package("sample", &info.version)
                .unwrap();
            external_db
                .add_file_ownership(&[PathBuf::from("/usr/bin/sample")], "sample")
                .unwrap();
            external_db.write_arkinfo(&info).unwrap();
            let paths = [
                "etc/arkpkg/package.db",
                "etc/arkpkg/file.db",
                "etc/arkpkg/packages/sample.arkinfo",
            ];
            let before: Vec<_> = paths
                .iter()
                .map(|p| fs::read(outside.path().join(p)).unwrap())
                .collect();
            fs::remove_dir_all(root.path().join(link)).unwrap();
            symlink(outside.path().join(link), root.path().join(link)).unwrap();

            assert!(db.read_installed_packages().is_err());
            assert!(db.add_installed_package("other", &info.version).is_err());
            assert!(db.remove_installed_package("sample").is_err());
            assert!(db.read_file_ownership().is_err());
            assert!(
                db.add_file_ownership(&[PathBuf::from("/usr/bin/other")], "other")
                    .is_err()
            );
            assert!(
                db.remove_file_ownership(&[PathBuf::from("/usr/bin/sample")])
                    .is_err()
            );
            assert!(db.read_arkinfo("sample").is_err());
            assert!(db.write_arkinfo(&info).is_err());
            assert!(db.remove_arkinfo("sample").is_err());
            for (path, contents) in paths.iter().zip(before) {
                assert_eq!(fs::read(outside.path().join(path)).unwrap(), contents);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_rejects_escaping_file_symlinks() {
        use std::os::unix::fs::symlink;

        for name in ["package.db", "file.db", "packages/sample.arkinfo"] {
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let db = Database::new(root.path());
            db.init().unwrap();
            let target = outside.path().join("state");
            let path = root.path().join("etc/arkpkg").join(name);
            let _ = fs::remove_file(&path);
            symlink(&target, &path).unwrap();
            if name.ends_with(".db") {
                assert!(db.init().is_err());
                assert!(!target.exists(), "created a dangling symlink target");
                fs::write(&target, "sample 1.0\n").unwrap();
                assert!(db.read_installed_packages().is_err());
                assert!(db.read_file_ownership().is_err());
            } else {
                let info = "Name: sample\nVersion: 1.0\n";
                fs::write(&target, info).unwrap();
                assert!(db.read_arkinfo("sample").is_err());
                db.write_arkinfo(&ArkInfo::parse(info).unwrap()).unwrap();
                assert!(
                    !fs::symlink_metadata(&path)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                fs::remove_file(&path).unwrap();
                symlink(&target, &path).unwrap();
                db.remove_arkinfo("sample").unwrap();
                assert!(!path.exists());
                assert_eq!(fs::read_to_string(&target).unwrap(), info);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_handles_remain_anchored_during_path_replacement() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let parent = TempDir::new().unwrap();
        let root = parent.path().join("root");
        let outside = TempDir::new().unwrap();
        let db = Database::new(&root);
        db.init().unwrap();
        let dir = db.packages_dir().unwrap();
        let mut temp = Database::temporary_file(&dir).unwrap();
        temp.write_all(b"private metadata").unwrap();
        fs::rename(
            root.join("etc/arkpkg/packages"),
            root.join("saved-packages"),
        )
        .unwrap();
        symlink(outside.path(), root.join("etc/arkpkg/packages")).unwrap();
        temp.replace("sample.arkinfo").unwrap();
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        let saved = root.join("saved-packages/sample.arkinfo");
        assert_eq!(fs::read_to_string(&saved).unwrap(), "private metadata");
        assert_eq!(
            fs::metadata(&saved).unwrap().permissions().mode() & 0o777,
            0o600
        );
        dir.remove_file("sample.arkinfo").unwrap();
        assert!(!saved.exists());

        fs::remove_file(root.join("etc/arkpkg/packages")).unwrap();
        let moved = parent.path().join("moved");
        fs::rename(&root, &moved).unwrap();
        symlink(outside.path(), &root).unwrap();
        db.clone()
            .add_installed_package("sample", &Version::parse("1.0").unwrap())
            .unwrap();
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        assert_eq!(
            fs::read_to_string(moved.join("etc/arkpkg/package.db")).unwrap(),
            "sample 1.0\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_supports_contained_relative_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("config")).unwrap();
        symlink("config", root.path().join("etc")).unwrap();
        let db = Database::new(root.path());
        let info = ArkInfo::parse("Name: sample\nVersion: 1.0\n").unwrap();
        db.add_installed_package("sample", &info.version).unwrap();
        db.write_arkinfo(&info).unwrap();
        assert_eq!(db.read_arkinfo("sample").unwrap(), info);
        db.remove_arkinfo("sample").unwrap();
        assert!(matches!(
            db.read_arkinfo("sample"),
            Err(ArkError::PackageNotFound(_))
        ));
        db.remove_arkinfo("sample").unwrap();

        let outside = TempDir::new_in(root.path().parent().unwrap()).unwrap();
        fs::remove_file(root.path().join("etc")).unwrap();
        let escape = Path::new("..").join(outside.path().file_name().unwrap());
        symlink(escape, root.path().join("etc")).unwrap();
        assert!(db.init().is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
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

        fs::remove_file(db.root().join("etc/arkpkg/file.db")).unwrap();
        fs::create_dir(db.root().join("etc/arkpkg/file.db")).unwrap();

        let added = PathBuf::from("/usr/bin/added");
        assert!(
            db.add_file_ownership_from_map(&mut ownership, &[existing, added], "replacement")
                .is_err()
        );
        assert_eq!(ownership, before);
    }

    #[test]
    fn rejects_ownership_paths_that_would_be_trimmed() {
        let root = TempDir::new().unwrap();
        let db = Database::new(root.path());
        db.add_file_ownership(&[PathBuf::from("/usr/bin/tool")], "owner")
            .unwrap();
        let mut ownership = db.read_file_ownership().unwrap();
        let before = ownership.clone();
        let contents = fs::read(root.path().join("etc/arkpkg/file.db")).unwrap();

        for suffix in [" ", "\u{00a0}", "\u{2003}"] {
            let path = PathBuf::from(format!("/usr/bin/tool{suffix}"));
            assert!(
                db.add_file_ownership_from_map(&mut ownership, &[path], "other")
                    .is_err()
            );
            assert_eq!(ownership, before);
            assert_eq!(
                fs::read(root.path().join("etc/arkpkg/file.db")).unwrap(),
                contents
            );
        }
    }

    #[test]
    fn rejects_unrepresentable_paths_in_all_arkinfo_records() {
        let root = TempDir::new().unwrap();
        let db = Database::new(root.path());
        let mut info =
            ArkInfo::parse("Name: sample\nVersion: 1.0\nFiles:\n/usr/bin/tool\n").unwrap();
        info.file_hashes = vec![(PathBuf::from("/usr/bin/tool"), "a".repeat(64))];
        info.preserved_files = vec![(
            PathBuf::from("/usr/bin/tool"),
            PathBuf::from("/usr/bin/.tmp-original"),
        )];
        db.write_arkinfo(&info).unwrap();
        let metadata = root.path().join("etc/arkpkg/packages/sample.arkinfo");
        let contents = fs::read(&metadata).unwrap();

        for suffix in [" ", "\u{00a0}", "\u{2003}"] {
            for field in 0..4 {
                let mut invalid = info.clone();
                let path = match field {
                    0 => &mut invalid.files[0],
                    1 => &mut invalid.file_hashes[0].0,
                    2 => &mut invalid.preserved_files[0].0,
                    _ => &mut invalid.preserved_files[0].1,
                };
                path.as_mut_os_string().push(suffix);
                assert!(db.write_arkinfo(&invalid).is_err());
                assert_eq!(fs::read(&metadata).unwrap(), contents);
            }
        }
    }

    #[test]
    fn old_arkinfo_without_preserved_files_remains_readable() {
        let arkinfo =
            ArkInfo::parse("Name: sample\nVersion: 1.0.0\nFiles:\n/usr/bin/sample\n").unwrap();
        assert!(arkinfo.preserved_files.is_empty());
        assert!(arkinfo.file_hashes.is_empty());
    }

    #[test]
    fn typed_file_hashes_round_trip_and_reject_malformed_records() {
        // A colon in a path must not be mistaken for part of the integrity tag.
        let path = "/usr/bin/sample:variant";
        let record = |hash: &str| {
            format!("Name: sample\nVersion: 1.0\nFiles:\n{path}\nFileHashes:\n{path}:{hash}\n")
        };
        for prefix in ["", "file-sha256=", "symlink-sha256="] {
            let hash = format!("{prefix}{}", "A".repeat(64));
            let info = ArkInfo::parse(&record(&hash)).unwrap();
            assert_eq!(info.file_hashes, vec![(PathBuf::from(path), hash)]);
            assert_eq!(ArkInfo::parse(&info.to_string_pretty()).unwrap(), info);
        }
        for hash in [
            format!("directory-sha256={}", "a".repeat(64)),
            format!("file-sha256={}", "a".repeat(63)),
            format!("symlink-sha256={}", "g".repeat(64)),
            format!("symlink-sha256={}", "a".repeat(65)),
            "file-sha256=".to_string(),
        ] {
            assert!(ArkInfo::parse(&record(&hash)).is_err(), "accepted {hash}");
        }
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
