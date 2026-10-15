use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use lz4_flex::frame::FrameDecoder;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use url::Url;

use crate::checksum::compute_sha256_file;
use crate::errors::{ArkError, Result};
use crate::metadata::{Metadata, validate_package_name};
use crate::version::Version;

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
    pub sha256: String,
}

/// Reads repository definitions and package indexes beneath the target root.
#[derive(Debug, Clone)]
pub struct RepositoryManager {
    root: PathBuf,
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

    /// Downloads and validates the newest compatible archive for a package.
    pub fn download_package(
        &self,
        package_name: &str,
        selected_repo: Option<&str>,
    ) -> Result<NamedTempFile> {
        validate_package_name(package_name)?;
        let configs = self.selected_repositories(selected_repo)?;
        let current_arch = std::env::consts::ARCH;
        let mut candidates = Vec::new();

        for config in configs {
            for package in self.fetch_index(&config)? {
                let arch_matches =
                    package.arch == current_arch || package.arch == "any" || package.arch == "all";
                if package.name == package_name && arch_matches {
                    let version = Version::parse(&package.version)?;
                    let arch_rank = u8::from(package.arch == current_arch);
                    candidates.push((version, arch_rank, config.clone(), package));
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
                "No compatible '{}' package was found in configured repositories",
                package_name
            ))
        })?;

        let package_url = format!("{}/{}", config.url.trim_end_matches('/'), package.file);
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

        let mut temporary = NamedTempFile::new()?;
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
    if package.sha256.len() != 64 || !package.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ArkError::InvalidPackage(format!(
            "Invalid SHA-256 digest for package '{} {}'",
            package.name, package.version
        )));
    }
    Ok(())
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
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| ArkError::InvalidPackage(format!("Invalid package archive: {error}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| {
            ArkError::InvalidPackage(format!("Invalid package archive: {error}"))
        })?;
        let entry_path = entry
            .path()
            .map_err(|error| ArkError::InvalidPackage(format!("Invalid archive path: {error}")))?;
        if entry_path == Path::new("ARKPKG") && entry.header().entry_type().is_file() {
            let mut content = String::new();
            entry.read_to_string(&mut content)?;
            return Metadata::parse(&content);
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
            sha256: "a".repeat(64),
        }
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
}
