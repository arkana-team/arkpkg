use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
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
    pub dependencies: Vec<DependencyReq>,
    pub provides: Vec<ProvideItem>,
    pub conflicts: Vec<String>,
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
        let mut dependencies = Vec::new();
        let mut provides = Vec::new();
        let mut conflicts = Vec::new();

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
                    _ => {
                        if let Some(sec) = current_section {
                            match sec {
                                "Files" => files.push(PathBuf::from(trimmed)),
                                "Dependencies" => dependencies.push(DependencyReq::parse(trimmed)?),
                                "Provides" => provides.push(ProvideItem::parse(trimmed)?),
                                "Conflicts" => conflicts.push(trimmed.to_string()),
                                _ => {}
                            }
                        }
                    }
                }
            } else if let Some(sec) = current_section {
                match sec {
                    "Files" => files.push(PathBuf::from(trimmed)),
                    "Dependencies" => dependencies.push(DependencyReq::parse(trimmed)?),
                    "Provides" => provides.push(ProvideItem::parse(trimmed)?),
                    "Conflicts" => conflicts.push(trimmed.to_string()),
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
            dependencies,
            provides,
            conflicts,
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

    /// Ensures that database directories exist.
    pub fn init(&self) -> Result<()> {
        let db_dir = self.root.join("etc/arkpkg");
        fs::create_dir_all(&db_dir)?;
        fs::create_dir_all(self.packages_dir())?;

        let log_dir = self.root.join("var/log");
        fs::create_dir_all(&log_dir)?;

        if !self.package_db_path().exists() {
            File::create(self.package_db_path())?;
        }
        if !self.file_db_path().exists() {
            File::create(self.file_db_path())?;
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
        validate_package_name(pkg_name)?;
        let mut map = self.read_file_ownership()?;
        for f in files {
            validate_installed_path(f)?;
            map.insert(f.clone(), pkg_name.to_string());
        }

        self.atomic_write_file_db(&map)
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

        let arkinfo = ArkInfo {
            name: "bash".into(),
            version: Version::parse("5.3.0").unwrap(),
            arch: "x86_64".into(),
            installed_at: "2026-07-29T12:00:00Z".into(),
            checksum: "abc123hash".into(),
            files: vec![file_path.clone()],
            dependencies: vec![DependencyReq::parse("glibc>=2.39").unwrap()],
            provides: vec![],
            conflicts: vec!["busybox".into()],
        };

        db.write_arkinfo(&arkinfo).unwrap();
        let loaded = db.read_arkinfo("bash").unwrap();
        assert_eq!(loaded.name, "bash");
        assert_eq!(loaded.files, vec![file_path]);
        assert_eq!(loaded.dependencies, arkinfo.dependencies);
        assert_eq!(loaded.conflicts, arkinfo.conflicts);
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

        let root = TempDir::new().unwrap();
        let db = Database::new(root.path());
        assert!(db.read_arkinfo("../outside").is_err());
    }
}
