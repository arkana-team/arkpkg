use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use lz4_flex::frame::FrameDecoder;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use url::Url;

use crate::archive::{ArchiveReader, MAX_DECODED_ARCHIVE_BYTES};
use crate::checksum::compute_sha256_file;
use crate::errors::{ArkError, Result};
use crate::metadata::{Metadata, read_metadata, validate_package_name};
use crate::version::{DependencyReq, Version};

const MAX_INDEX_SIZE: u64 = 16 * 1024 * 1024;
const MAX_PACKAGE_SIZE: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryConfig {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct RepositoryIndex {
    schema_version: u32,
    packages: Vec<RepositoryPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepositoryPackage {
    pub name: String,
    pub version: String,
    pub arch: String,
    #[serde(default)]
    pub description: String,
    pub file: String,
    /// Optional absolute HTTPS URL for repositories hosted separately from their index.
    #[serde(default)]
    pub download_url: Option<String>,
    pub sha256: String,
}

/// Reads repository definitions and package indexes beneath the target root.
#[derive(Debug, Clone)]
pub struct RepositoryManager {
    root: PathBuf,
}

/// A validated snapshot of the configured repository indexes.
#[derive(Debug, Clone)]
pub struct RepositoryCatalog {
    root: PathBuf,
    repositories: Vec<(RepositoryConfig, Vec<RepositoryPackage>)>,
}

impl RepositoryManager {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn repositories_dir(&self) -> PathBuf {
        self.root.join("etc/arkpkg/repos.d")
    }

    fn config_path(&self, name: &str) -> Result<PathBuf> {
        validate_package_name(name)?;
        Ok(self.repositories_dir().join(format!("{name}.json")))
    }

    pub fn add(&self, name: &str, url: &str) -> Result<()> {
        let path = self.config_path(name)?;
        let config = RepositoryConfig {
            name: name.to_string(),
            url: validate_repository_url(url)?,
        };
        fs::create_dir_all(self.repositories_dir())?;
        if path.exists() {
            return Err(ArkError::Generic(format!(
                "Repository '{}' already exists",
                name
            )));
        }

        let mut temporary = NamedTempFile::new_in(self.repositories_dir())?;
        serde_json::to_writer_pretty(&mut temporary, &config).map_err(|error| {
            ArkError::Generic(format!("Could not encode repository config: {error}"))
        })?;
        temporary.write_all(b"\n")?;
        temporary
            .persist(path)
            .map_err(|error| ArkError::Io(error.error))?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let path = self.config_path(name)?;
        if !path.exists() {
            return Err(ArkError::PackageNotFound(format!(
                "Repository '{}' is not configured",
                name
            )));
        }
        fs::remove_file(path)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<RepositoryConfig>> {
        let directory = self.repositories_dir();
        if !directory.exists() {
            return Ok(Vec::new());
        }

        let mut configs = Vec::new();
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let config: RepositoryConfig =
                serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
                    ArkError::InvalidPackage(format!(
                        "Invalid repository config '{}': {error}",
                        path.display()
                    ))
                })?;
            validate_package_name(&config.name)?;
            let normalized_url = validate_repository_url(&config.url)?;
            if normalized_url != config.url {
                return Err(ArkError::InvalidPackage(format!(
                    "Repository config '{}' URL is not normalized",
                    path.display()
                )));
            }
            configs.push(config);
        }
        configs.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(configs)
    }

    pub fn search(
        &self,
        query: &str,
        selected_repo: Option<&str>,
    ) -> Result<Vec<(String, RepositoryPackage)>> {
        if selected_repo.is_none() && self.list()?.is_empty() {
            return Ok(Vec::new());
        }
        let configs = self.selected_repositories(selected_repo)?;
        let query = query.to_lowercase();
        let mut matches = Vec::new();
        for config in configs {
            for package in self.fetch_index(&config)? {
                if package.name.to_lowercase().contains(&query)
                    || package.description.to_lowercase().contains(&query)
                {
                    matches.push((config.name.clone(), package));
                }
            }
        }
        matches.sort_by(|left, right| {
            left.1
                .name
                .cmp(&right.1.name)
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| left.1.version.cmp(&right.1.version))
        });
        Ok(matches)
    }

    /// Fetches and validates each selected repository index once for a package operation.
    pub fn catalog(&self, selected_repo: Option<&str>) -> Result<RepositoryCatalog> {
        let repositories = self
            .selected_repositories(selected_repo)?
            .into_iter()
            .map(|config| {
                let packages = self.fetch_index(&config)?;
                Ok((config, packages))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(RepositoryCatalog {
            root: self.root.clone(),
            repositories,
        })
    }

    /// Downloads and validates the newest compatible archive for a package.
    pub fn download_package(
        &self,
        package_name: &str,
        selected_repo: Option<&str>,
    ) -> Result<NamedTempFile> {
        validate_package_name(package_name)?;
        let requirement = DependencyReq::parse(package_name)?;
        self.catalog(selected_repo)?
            .download_package_matching(&requirement)
    }
}

impl RepositoryCatalog {
    /// Downloads the newest compatible archive satisfying a package requirement.
    pub fn download_package_matching(&self, requirement: &DependencyReq) -> Result<NamedTempFile> {
        let (config, package) = self.best_candidate(requirement)?;

        let package_url = package.download_url.as_deref().map_or_else(
            || format!("{}/{}", config.url.trim_end_matches('/'), package.file),
            str::to_owned,
        );
        let response = request(&package_url, MAX_PACKAGE_SIZE)?;
        let content_length = response
            .header("Content-Length")
            .and_then(|value| value.parse::<u64>().ok());
        if content_length.is_some_and(|length| length > MAX_PACKAGE_SIZE) {
            return Err(ArkError::InvalidPackage(format!(
                "Package archive exceeds the {} byte download limit",
                MAX_PACKAGE_SIZE
            )));
        }

        let mut temporary = self.download_tempfile()?;
        let copied = std::io::copy(
            &mut response.into_reader().take(MAX_PACKAGE_SIZE + 1),
            &mut temporary,
        )?;
        if copied > MAX_PACKAGE_SIZE {
            return Err(ArkError::InvalidPackage(format!(
                "Package archive exceeds the {} byte download limit",
                MAX_PACKAGE_SIZE
            )));
        }
        temporary.flush()?;

        let checksum = compute_sha256_file(temporary.path())?;
        if !checksum.eq_ignore_ascii_case(&package.sha256) {
            return Err(ArkError::VerificationFailed(format!(
                "SHA-256 mismatch for {} {} from repository '{}'",
                package.name, package.version, config.name
            )));
        }

        let metadata = read_archive_metadata(temporary.path())?;
        if metadata.name != package.name
            || metadata.version.as_str() != package.version
            || metadata.arch != package.arch
        {
            return Err(ArkError::VerificationFailed(format!(
                "Repository index does not match the embedded metadata for {} {}",
                package.name, package.version
            )));
        }

        Ok(temporary)
    }

    fn download_tempfile(&self) -> Result<NamedTempFile> {
        rooted_download_tempfile(&self.root)
    }

    fn best_candidate(
        &self,
        requirement: &DependencyReq,
    ) -> Result<(&RepositoryConfig, &RepositoryPackage)> {
        validate_package_name(&requirement.name)?;
        let current_arch = std::env::consts::ARCH;
        let mut candidates = Vec::new();

        for (config, packages) in &self.repositories {
            for package in packages {
                let arch_matches =
                    package.arch == current_arch || package.arch == "any" || package.arch == "all";
                if package.name.eq_ignore_ascii_case(&requirement.name) && arch_matches {
                    let version = Version::parse(&package.version)?;
                    if !requirement.is_satisfied_by(&version) {
                        continue;
                    }
                    let arch_rank = u8::from(package.arch == current_arch);
                    candidates.push((version, arch_rank, config, package));
                }
            }
        }

        candidates.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| right.2.name.cmp(&left.2.name))
        });
        let (_, _, config, package) = candidates.pop().ok_or_else(|| {
            ArkError::PackageNotFound(format!(
                "No compatible package satisfying '{}' was found in configured repositories",
                requirement
            ))
        })?;
        Ok((config, package))
    }
}

impl RepositoryManager {
    fn selected_repositories(&self, selected_repo: Option<&str>) -> Result<Vec<RepositoryConfig>> {
        let configs = self.list()?;
        if let Some(name) = selected_repo {
            return configs
                .into_iter()
                .find(|config| config.name == name)
                .map(|config| vec![config])
                .ok_or_else(|| {
                    ArkError::PackageNotFound(format!("Repository '{}' is not configured", name))
                });
        }
        if configs.is_empty() {
            return Err(ArkError::PackageNotFound(
                "No package repositories are configured. Add one with 'arkpkg repo add'.".into(),
            ));
        }
        Ok(configs)
    }

    fn fetch_index(&self, config: &RepositoryConfig) -> Result<Vec<RepositoryPackage>> {
        let index_url = format!("{}/index.json", config.url.trim_end_matches('/'));
        let response = request(&index_url, MAX_INDEX_SIZE)?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_INDEX_SIZE + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_INDEX_SIZE {
            return Err(ArkError::InvalidPackage(format!(
                "Repository index at '{}' exceeds the size limit",
                index_url
            )));
        }
        let index: RepositoryIndex = serde_json::from_slice(&bytes).map_err(|error| {
            ArkError::InvalidPackage(format!(
                "Invalid repository index at '{index_url}': {error}"
            ))
        })?;
        if index.schema_version != 1 {
            return Err(ArkError::InvalidPackage(format!(
                "Unsupported repository index schema version {} at '{}'",
                index.schema_version, index_url
            )));
        }
        for package in &index.packages {
            validate_package(package)?;
        }
        Ok(index.packages)
    }
}

fn rooted_download_tempfile(root: &Path) -> Result<NamedTempFile> {
    let canonical_root = root.canonicalize()?;
    let mut cache_dir = canonical_root.clone();
    for component in ["var", "cache", "arkpkg", "downloads"] {
        cache_dir.push(component);
        // Check each existing component before creating anything beneath it.
        match fs::create_dir(&cache_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        cache_dir = cache_dir.canonicalize()?;
        if !cache_dir.starts_with(&canonical_root) {
            return Err(ArkError::InvalidPackage(format!(
                "Package download cache '{}' resolves outside target root '{}'",
                cache_dir.display(),
                canonical_root.display()
            )));
        }
        if !cache_dir.is_dir() {
            return Err(ArkError::InvalidPackage(format!(
                "Package download cache '{}' is not a directory",
                cache_dir.display()
            )));
        }
    }
    Ok(NamedTempFile::new_in(cache_dir)?)
}

fn validate_repository_url(value: &str) -> Result<String> {
    let parsed = Url::parse(value.trim()).map_err(|error| {
        ArkError::InvalidPackage(format!("Invalid repository URL '{value}': {error}"))
    })?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ArkError::InvalidPackage(
            "Repository URLs must be HTTPS URLs without credentials, query, or fragment".into(),
        ));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

fn validate_package(package: &RepositoryPackage) -> Result<()> {
    validate_package_name(&package.name)?;
    Version::parse(&package.version)?;
    if package.arch.is_empty()
        || !package
            .arch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
    {
        return Err(ArkError::InvalidPackage(format!(
            "Invalid architecture '{}' in repository index",
            package.arch
        )));
    }

    let path = Path::new(&package.file);
    if path.is_absolute()
        || !package.file.starts_with("packages/")
        || !package.file.is_ascii()
        || !package
            .file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-/".contains(&byte))
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ArkError::InvalidPackage(format!(
            "Unsafe package path '{}' in repository index",
            package.file
        )));
    }
    if let Some(url) = &package.download_url {
        validate_download_url(url)?;
    }
    if package.sha256.len() != 64 || !package.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ArkError::InvalidPackage(format!(
            "Invalid SHA-256 digest for package '{} {}'",
            package.name, package.version
        )));
    }
    Ok(())
}

fn validate_download_url(value: &str) -> Result<String> {
    let parsed = Url::parse(value).map_err(|error| {
        ArkError::InvalidPackage(format!("Invalid package download URL '{value}': {error}"))
    })?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ArkError::InvalidPackage(
            "Package download URLs must use HTTPS without credentials, query, or fragment".into(),
        ));
    }
    Ok(parsed.to_string())
}

fn request(url: &str, limit: u64) -> Result<ureq::Response> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .build();
    let response = agent
        .get(url)
        .call()
        .map_err(|error| ArkError::Generic(format!("Request to '{url}' failed: {error}")))?;
    let final_url = Url::parse(response.get_url()).map_err(|error| {
        ArkError::Generic(format!("Invalid response URL from '{url}': {error}"))
    })?;
    if final_url.scheme() != "https" {
        return Err(ArkError::Generic(format!(
            "Refusing insecure redirect while requesting '{url}'"
        )));
    }
    if response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit)
    {
        return Err(ArkError::InvalidPackage(format!(
            "Response from '{url}' exceeds the {limit} byte size limit"
        )));
    }
    Ok(response)
}

fn read_archive_metadata(path: &Path) -> Result<Metadata> {
    let file = File::open(path)?;
    let decoder = FrameDecoder::new(file);
    let mut archive = tar::Archive::new(ArchiveReader::new(decoder, MAX_DECODED_ARCHIVE_BYTES));
    let entries = archive
        .entries()
        .map_err(|error| ArkError::InvalidPackage(format!("Invalid package archive: {error}")))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            ArkError::InvalidPackage(format!("Invalid package archive: {error}"))
        })?;
        let entry_path = entry
            .path()
            .map_err(|error| ArkError::InvalidPackage(format!("Invalid archive path: {error}")))?;
        if entry_path == Path::new("ARKPKG") && entry.header().entry_type().is_file() {
            let size = entry.size();
            return read_metadata(entry, size);
        }
    }
    Err(ArkError::InvalidPackage(
        "Package archive is missing its ARKPKG metadata".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn package(file: &str) -> RepositoryPackage {
        RepositoryPackage {
            name: "hello".into(),
            version: "1.2.3".into(),
            arch: "any".into(),
            description: "hello package".into(),
            file: file.into(),
            download_url: None,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn repository_inspection_enforces_metadata_size_limit() {
        let directory = TempDir::new().unwrap();
        let archive_path = directory.path().join("metadata.ark");
        for size in [1024 * 1024, 1024 * 1024 + 1] {
            let mut metadata =
                b"Name: bounded\nDescription: test\nVersion: 1.0\nArch: any\n#".to_vec();
            metadata.resize(size, b'x');
            let mut archive = tar::Builder::new(lz4_flex::frame::FrameEncoder::new(
                File::create(&archive_path).unwrap(),
            ));
            let mut header = tar::Header::new_gnu();
            header.set_path("ARKPKG").unwrap();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(metadata.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append(&header, metadata.as_slice()).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
            assert!(fs::metadata(&archive_path).unwrap().len() < 16 * 1024);

            let result = read_archive_metadata(&archive_path);
            if size == 1024 * 1024 {
                assert_eq!(result.unwrap().name, "bounded");
            } else {
                assert!(
                    matches!(result, Err(ArkError::InvalidPackage(message)) if message.contains("size limit"))
                );
            }
        }
    }

    #[test]
    fn package_selection_respects_version_and_architecture_requirements() {
        let package = |version: &str, arch: &str| RepositoryPackage {
            name: "hello".into(),
            version: version.into(),
            arch: arch.into(),
            description: "hello package".into(),
            file: format!("packages/hello-{version}.ark"),
            download_url: None,
            sha256: "a".repeat(64),
        };
        let root = TempDir::new().unwrap();
        let catalog = RepositoryCatalog {
            root: root.path().to_path_buf(),
            repositories: vec![(
                RepositoryConfig {
                    name: "main".into(),
                    url: "https://example.com/arkpkg".into(),
                },
                vec![
                    package("1.0", "any"),
                    package("2.0", "any"),
                    package("3.0", "other-arch"),
                ],
            )],
        };

        let (config, selected) = catalog
            .best_candidate(&DependencyReq::parse("hello>=1.5").unwrap())
            .unwrap();
        assert_eq!(config.name, "main");
        assert_eq!(selected.version, "2.0");
        assert!(
            catalog
                .best_candidate(&DependencyReq::parse("hello>=3.0").unwrap())
                .is_err()
        );
    }

    #[test]
    fn repository_urls_require_https_and_drop_trailing_slashes() {
        assert_eq!(
            validate_repository_url("https://example.com/arkpkg/").unwrap(),
            "https://example.com/arkpkg"
        );
        assert!(validate_repository_url("http://example.com").is_err());
        assert!(validate_repository_url("https://user:pass@example.com").is_err());
    }

    #[test]
    fn rejects_unsafe_index_paths_and_invalid_hashes() {
        for path in [
            "packages/../../etc/passwd",
            "https://other.invalid/file.ark",
            "/packages/a.ark",
        ] {
            assert!(validate_package(&package(path)).is_err(), "accepted {path}");
        }
        let mut invalid_hash = package("packages/hello.ark");
        invalid_hash.sha256 = "xyz".into();
        assert!(validate_package(&invalid_hash).is_err());
    }

    #[test]
    fn accepts_secure_direct_download_urls_and_rejects_unsafe_ones() {
        let mut direct = package("packages/hello.ark");
        direct.download_url = Some(
            "https://raw.githubusercontent.com/arkana-team/arkpkg-repo/main/packages/hello.ark"
                .into(),
        );
        assert!(validate_package(&direct).is_ok());

        for url in [
            "http://example.com/hello.ark",
            "https://user:pass@example.com/hello.ark",
            "https://example.com/hello.ark?token=secret",
            "https://example.com/hello.ark#fragment",
        ] {
            direct.download_url = Some(url.into());
            assert!(validate_package(&direct).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn old_repository_index_entries_without_download_url_remain_valid() {
        let json = r#"{"name":"hello","version":"1.2.3","arch":"any","description":"hello package","file":"packages/hello.ark","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
        let parsed: RepositoryPackage = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.download_url, None);
        assert!(validate_package(&parsed).is_ok());
    }

    #[test]
    fn repository_configs_are_added_listed_and_removed_under_the_target_root() {
        let root = TempDir::new().unwrap();
        let repositories = RepositoryManager::new(root.path());
        repositories
            .add("main", "https://example.com/arkpkg/")
            .unwrap();
        assert_eq!(
            repositories.list().unwrap()[0].url,
            "https://example.com/arkpkg"
        );
        repositories.remove("main").unwrap();
        assert!(repositories.list().unwrap().is_empty());
    }

    #[test]
    fn package_download_tempfiles_use_the_target_root_cache() {
        let root = TempDir::new().unwrap();
        let catalog = RepositoryCatalog {
            root: root.path().to_path_buf(),
            repositories: Vec::new(),
        };

        let temporary = catalog.download_tempfile().unwrap();

        assert!(
            temporary.path().starts_with(
                root.path()
                    .join("var/cache/arkpkg/downloads")
                    .canonicalize()
                    .unwrap()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_download_cache_rejects_symlinks_outside_the_target_root() {
        use std::os::unix::fs::symlink;

        for component in [
            "var",
            "var/cache",
            "var/cache/arkpkg",
            "var/cache/arkpkg/downloads",
        ] {
            for dangling in [false, true] {
                let root = TempDir::new().unwrap();
                let outside = TempDir::new().unwrap();
                let target = if dangling {
                    outside.path().join("missing")
                } else {
                    outside.path().to_path_buf()
                };
                let link = root.path().join(component);
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                symlink(target, link).unwrap();

                assert!(rooted_download_tempfile(root.path()).is_err());
                assert_eq!(
                    fs::read_dir(outside.path()).unwrap().count(),
                    0,
                    "{component}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn package_download_cache_rejects_installed_absolute_symlink_before_creating_children() {
        use crate::archive::create_ark;
        use crate::database::Database;
        use crate::installer::Installer;
        use crate::logger::Logger;
        use crate::prompt::PromptHandler;
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(
            source.path().join("ARKPKG"),
            "Name: cachelink\nDescription: cache symlink\nVersion: 1.0\nArch: any\n",
        )
        .unwrap();
        let cache_parent = source.path().join("package/var/cache");
        fs::create_dir_all(&cache_parent).unwrap();
        symlink(outside.path(), cache_parent.join("arkpkg")).unwrap();
        let archive = source.path().join("cachelink.ark");
        create_ark(source.path(), &archive).unwrap();
        let database = Database::new(root.path());
        let logger = Logger::new(root.path());
        logger.init().unwrap();
        let mut installer =
            Installer::new(&database, &logger, PromptHandler::new(true, false, false));
        installer.install(&archive).unwrap();

        assert_eq!(
            fs::read_link(root.path().join("var/cache/arkpkg")).unwrap(),
            outside.path()
        );
        assert!(rooted_download_tempfile(root.path()).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn package_download_cache_allows_symlinks_inside_the_target_root() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("cache-storage")).unwrap();
        fs::create_dir(root.path().join("var")).unwrap();
        symlink("../cache-storage", root.path().join("var/cache")).unwrap();

        let mut temporary = rooted_download_tempfile(root.path()).unwrap();
        temporary.write_all(b"download").unwrap();
        assert_eq!(fs::read(temporary.path()).unwrap(), b"download");
        assert_eq!(
            temporary.path().parent().unwrap(),
            root.path()
                .join("cache-storage/arkpkg/downloads")
                .canonicalize()
                .unwrap()
        );
    }
}
