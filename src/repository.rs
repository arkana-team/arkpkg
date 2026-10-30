use cap_std::ambient_authority;
use cap_std::fs::Dir;
use cap_tempfile::TempFile;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
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
// Repository metadata must finish within this decoded tar search budget.
// The package builder writes ARKPKG before the payload.
const MAX_METADATA_SCAN_SIZE: u64 = 16 * 1024 * 1024;
const REPOSITORIES_DIR: &str = "etc/arkpkg/repos.d";

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
    root_dir: Arc<OnceLock<Dir>>,
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
            root_dir: Arc::new(OnceLock::new()),
        }
    }

    fn repositories_dir(&self, create: bool) -> std::io::Result<Dir> {
        // Only the user-selected root is opened with ambient authority. Retain
        // its handle across calls and clones so pathname replacement cannot redirect IO.
        if self.root_dir.get().is_none() {
            if create {
                fs::create_dir_all(&self.root)?;
            }
            let dir = Dir::open_ambient_dir(&self.root, ambient_authority())?;
            let _ = self.root_dir.set(dir);
        }
        let root = self.root_dir.get().expect("root directory initialized");
        if create {
            root.create_dir_all(REPOSITORIES_DIR)?;
        }
        root.open_dir(REPOSITORIES_DIR)
    }

    fn config_name(name: &str) -> Result<String> {
        validate_package_name(name)?;
        Ok(format!("{name}.json"))
    }

    pub fn add(&self, name: &str, url: &str) -> Result<()> {
        let filename = Self::config_name(name)?;
        let config = RepositoryConfig {
            name: name.to_string(),
            url: validate_repository_url(url)?,
        };
        let directory = self.repositories_dir(true)?;
        match directory.symlink_metadata(&filename) {
            Ok(_) => {
                return Err(ArkError::Generic(format!(
                    "Repository '{}' already exists",
                    name
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let mut temporary = TempFile::new(&directory)?;
        // Preserve NamedTempFile's private permissions before writing data.
        #[cfg(unix)]
        {
            use cap_std::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(cap_std::fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer_pretty(&mut temporary, &config).map_err(|error| {
            ArkError::Generic(format!("Could not encode repository config: {error}"))
        })?;
        temporary.write_all(b"\n")?;
        publish_new_config(temporary, &directory, &filename).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ArkError::Generic(format!("Repository '{}' already exists", name))
            } else {
                error.into()
            }
        })?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let filename = Self::config_name(name)?;
        match self
            .repositories_dir(false)
            .and_then(|directory| directory.remove_file(filename))
        {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(
                ArkError::PackageNotFound(format!("Repository '{}' is not configured", name)),
            ),
            Err(error) => Err(error.into()),
        }
    }

    pub fn list(&self) -> Result<Vec<RepositoryConfig>> {
        let directory = match self.repositories_dir(false) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        let mut configs = Vec::new();
        for entry in directory.entries()? {
            let path = PathBuf::from(entry?.file_name());
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let config: RepositoryConfig = serde_json::from_slice(&directory.read(&path)?)
                .map_err(|error| {
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

#[cfg(any(target_os = "linux", target_os = "android"))]
fn publish_new_config(
    temporary: TempFile<'_>,
    directory: &Dir,
    filename: &str,
) -> std::io::Result<()> {
    use rustix::fs::AtFlags;

    // Linking the open file descriptor publishes the completed config in one
    // operation and fails if another add has already created this name.
    let proc_self_fd = rustix_linux_procfs::proc_self_fd().map_err(std::io::Error::from)?;
    let descriptor = rustix::path::DecInt::from_fd(temporary.as_file());
    rustix::fs::linkat(
        proc_self_fd,
        descriptor.as_c_str(),
        directory,
        filename,
        AtFlags::SYMLINK_FOLLOW,
    )
    .map_err(Into::into)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn publish_new_config(
    mut temporary: TempFile<'_>,
    directory: &Dir,
    filename: &str,
) -> std::io::Result<()> {
    use std::io::Seek;

    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut destination = directory.open_with(filename, &options)?;
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        destination.set_permissions(cap_std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.rewind()?;
    std::io::copy(&mut temporary, &mut destination)?;
    Ok(())
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
        .https_only(true)
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

pub(crate) fn read_archive_metadata(path: &Path) -> Result<Metadata> {
    let file = File::open(path)?;
    let decoder = FrameDecoder::new(file);
    // Limit below tar so skipped payloads and GNU/PAX extension records also
    // count toward the decoded search budget, while retaining the archive-wide
    // reader limit for the shared package parser.
    let bounded = ArchiveReader::new(decoder, MAX_DECODED_ARCHIVE_BYTES);
    let mut archive = tar::Archive::new(bounded.take(MAX_METADATA_SCAN_SIZE));
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
            // Entry::size includes PAX size overrides, unlike the raw header.
            let size = entry.size();
            return read_metadata(entry, size);
        }
    }
    if archive.into_inner().limit() == 0 {
        return Err(ArkError::InvalidPackage(format!(
            "ARKPKG metadata was not found within the {MAX_METADATA_SCAN_SIZE} byte decoded search limit"
        )));
    }
    Err(ArkError::InvalidPackage(
        "Package archive is missing its ARKPKG metadata".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::MAX_METADATA_SIZE;
    use lz4_flex::frame::FrameEncoder;
    use tempfile::TempDir;

    const TEST_METADATA: &[u8] = b"Name: hello\nDescription: test\nVersion: 1.2.3\nArch: any\n";

    fn metadata_archive(
        build: impl FnOnce(&mut tar::Builder<FrameEncoder<&mut File>>),
    ) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        let mut archive = tar::Builder::new(FrameEncoder::new(file.as_file_mut()));
        build(&mut archive);
        archive.into_inner().unwrap().finish().unwrap();
        file
    }

    fn append_test_entry<W: Write>(
        archive: &mut tar::Builder<W>,
        path: &str,
        kind: tar::EntryType,
        size: u64,
        data: impl Read,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append(&header, data).unwrap();
    }

    #[test]
    fn archive_metadata_enforces_decoded_size_limit() {
        for size in [
            TEST_METADATA.len() as u64,
            MAX_METADATA_SIZE - 1,
            MAX_METADATA_SIZE,
            MAX_METADATA_SIZE + 1,
        ] {
            let file = metadata_archive(|archive| {
                append_test_entry(
                    archive,
                    "ARKPKG",
                    tar::EntryType::Regular,
                    size,
                    TEST_METADATA.chain(std::io::repeat(b' ')).take(size),
                );
            });
            // Highly compressible metadata passes the compressed download limit.
            assert!(file.as_file().metadata().unwrap().len() < MAX_METADATA_SIZE);
            let result = read_archive_metadata(file.path());
            if size <= MAX_METADATA_SIZE {
                assert_eq!(result.unwrap().name, "hello");
            } else {
                assert!(
                    matches!(result, Err(ArkError::InvalidPackage(message)) if message.contains("size limit"))
                );
            }
        }
    }

    #[test]
    fn archive_metadata_rejects_oversized_pax_size_before_reading_body() {
        let file = metadata_archive(|archive| {
            let size = (MAX_METADATA_SIZE + 1).to_string();
            archive
                .append_pax_extensions([("size", size.as_bytes())])
                .unwrap();
            // The small raw header size must not bypass the PAX override.
            append_test_entry(
                archive,
                "ARKPKG",
                tar::EntryType::Regular,
                TEST_METADATA.len() as u64,
                TEST_METADATA,
            );
        });
        assert!(
            matches!(read_archive_metadata(file.path()), Err(ArkError::InvalidPackage(message)) if message.contains("size limit"))
        );
    }

    #[test]
    fn archive_metadata_search_limit_includes_skipped_payload_and_metadata() {
        // Two 512-byte headers plus 1024 bytes of metadata end exactly at the
        // budget. Moving metadata later must reject even a parseable prefix.
        for extra in [0, 512, 1024, 1536] {
            let file = metadata_archive(|archive| {
                let payload_size = MAX_METADATA_SCAN_SIZE - 2048 + extra;
                append_test_entry(
                    archive,
                    "package/payload",
                    tar::EntryType::Regular,
                    payload_size,
                    std::io::repeat(0).take(payload_size),
                );
                append_test_entry(
                    archive,
                    "ARKPKG",
                    tar::EntryType::Regular,
                    1024,
                    TEST_METADATA.chain(std::io::repeat(b' ')).take(1024),
                );
            });
            let result = read_archive_metadata(file.path());
            if extra == 0 {
                assert_eq!(result.unwrap().name, "hello");
            } else {
                assert!(
                    result.is_err(),
                    "accepted metadata beyond the search budget: {extra}"
                );
            }
        }
    }

    #[test]
    fn archive_metadata_search_limit_includes_tar_extensions() {
        let file = metadata_archive(|archive| {
            append_test_entry(
                archive,
                "././@LongLink",
                tar::EntryType::GNULongLink,
                MAX_METADATA_SCAN_SIZE,
                std::io::repeat(0).take(MAX_METADATA_SCAN_SIZE),
            );
            append_test_entry(
                archive,
                "ARKPKG",
                tar::EntryType::Regular,
                TEST_METADATA.len() as u64,
                TEST_METADATA,
            );
        });
        assert!(read_archive_metadata(file.path()).is_err());
    }

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
    fn requests_reject_http_before_connecting() {
        let error = request("http://127.0.0.1:0/index.json", MAX_INDEX_SIZE).unwrap_err();
        assert!(
            error.to_string().contains("https_only"),
            "expected HTTPS enforcement before connecting, got: {error}"
        );
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn repository_config_publication_does_not_replace_existing_file() {
        let root = TempDir::new().unwrap();
        let existing_contents = br#"{"name":"main","url":"https://old.example"}"#;
        fs::write(root.path().join("main.json"), existing_contents).unwrap();
        let directory = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let mut temporary = TempFile::new(&directory).unwrap();
        temporary.write_all(b"replacement config").unwrap();

        let error = publish_new_config(temporary, &directory, "main.json").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(root.path().join("main.json")).unwrap(),
            existing_contents
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn repository_configs_reject_escaping_directory_symlinks() {
        use std::os::unix::fs::symlink;

        for component in ["etc", "etc/arkpkg", "etc/arkpkg/repos.d"] {
            for relative in [false, true] {
                let parent = TempDir::new().unwrap();
                let root = parent.path().join("root");
                let outside = parent.path().join("outside");
                let link = root.join(component);
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                fs::create_dir(&outside).unwrap();
                let target = if relative {
                    PathBuf::from("../".repeat(Path::new(component).components().count()))
                        .join("outside")
                } else {
                    outside.clone()
                };
                symlink(target, &link).unwrap();
                let configs = outside.join(
                    Path::new("etc/arkpkg/repos.d")
                        .strip_prefix(component)
                        .unwrap(),
                );
                fs::create_dir_all(&configs).unwrap();
                let existing = configs.join("existing.json");
                let contents = br#"{"name":"existing","url":"https://example.com"}"#;
                fs::write(&existing, contents).unwrap();
                let repositories = RepositoryManager::new(&root);

                assert!(repositories.list().is_err(), "listed through {component}");
                assert!(repositories.add("new", "https://example.com").is_err());
                assert!(repositories.remove("existing").is_err());
                assert_eq!(fs::read(&existing).unwrap(), contents);
                assert_eq!(fs::read_dir(&configs).unwrap().count(), 1);

                fs::remove_dir_all(&outside).unwrap();
                assert!(repositories.add("new", "https://example.com").is_err());
                assert!(!outside.exists(), "created dangling target for {component}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn repository_configs_reject_installed_absolute_directory_symlink() {
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
            "Name: repolink\nDescription: repository symlink\nVersion: 1.0\nArch: any\n",
        )
        .unwrap();
        let config_parent = source.path().join("package/etc/arkpkg");
        fs::create_dir_all(&config_parent).unwrap();
        symlink(outside.path(), config_parent.join("repos.d")).unwrap();
        let archive = source.path().join("repolink.ark");
        create_ark(source.path(), &archive).unwrap();
        let database = Database::new(root.path());
        let logger = Logger::new(root.path());
        logger.init().unwrap();
        let mut installer =
            Installer::new(&database, &logger, PromptHandler::new(true, false, false));
        installer.install(&archive).unwrap();
        let existing = outside.path().join("existing.json");
        let contents = br#"{"name":"existing","url":"https://example.com"}"#;
        fs::write(&existing, contents).unwrap();
        let repositories = RepositoryManager::new(root.path());

        assert!(repositories.add("new", "https://example.com").is_err());
        assert!(repositories.list().is_err());
        assert!(repositories.remove("existing").is_err());
        assert_eq!(fs::read(existing).unwrap(), contents);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn repository_configs_reject_escaping_file_symlinks() {
        use std::os::unix::fs::symlink;

        for dangling in [false, true] {
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let directory = root.path().join(REPOSITORIES_DIR);
            fs::create_dir_all(&directory).unwrap();
            let target = outside.path().join("config.json");
            let contents = br#"{"name":"main","url":"https://example.com"}"#;
            if !dangling {
                fs::write(&target, contents).unwrap();
            }
            let link = directory.join("main.json");
            symlink(&target, &link).unwrap();
            let repositories = RepositoryManager::new(root.path());

            assert!(repositories.list().is_err());
            assert!(repositories.add("main", "https://example.org").is_err());
            assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
            repositories.remove("main").unwrap();
            assert!(fs::symlink_metadata(link).is_err());
            if dangling {
                assert!(!target.exists());
            } else {
                assert_eq!(fs::read(target).unwrap(), contents);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn repository_configs_remain_anchored_after_root_replacement() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let parent = TempDir::new().unwrap();
        let root = parent.path().join("root");
        let moved = parent.path().join("moved");
        let outside = TempDir::new().unwrap();
        let repositories = RepositoryManager::new(&root);
        assert!(repositories.list().unwrap().is_empty());
        assert!(matches!(
            repositories.remove("main"),
            Err(ArkError::PackageNotFound(_))
        ));
        repositories.add("main", "https://example.com").unwrap();
        fs::rename(&root, &moved).unwrap();
        symlink(outside.path(), &root).unwrap();

        let cloned = repositories.clone();
        cloned.add("other", "https://example.org").unwrap();
        assert_eq!(cloned.list().unwrap().len(), 2);
        assert_eq!(
            fs::metadata(moved.join(REPOSITORIES_DIR).join("other.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        cloned.remove("main").unwrap();
        assert!(!moved.join(REPOSITORIES_DIR).join("main.json").exists());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn repository_configs_reject_replaced_directory_ancestry() {
        use std::os::unix::fs::symlink;

        for component in ["etc", "etc/arkpkg", "etc/arkpkg/repos.d"] {
            let root = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let repositories = RepositoryManager::new(root.path());
            repositories.add("main", "https://example.com").unwrap();
            let external = RepositoryManager::new(outside.path());
            external.add("main", "https://example.org").unwrap();
            let path = outside.path().join(REPOSITORIES_DIR).join("main.json");
            let before = fs::read(&path).unwrap();
            fs::remove_dir_all(root.path().join(component)).unwrap();
            symlink(outside.path().join(component), root.path().join(component)).unwrap();

            assert!(repositories.list().is_err());
            assert!(repositories.add("new", "https://example.com").is_err());
            assert!(repositories.remove("main").is_err());
            assert_eq!(fs::read(path).unwrap(), before);
            assert_eq!(external.list().unwrap().len(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn repository_configs_support_contained_relative_directory_symlinks() {
        use std::os::unix::fs::symlink;

        for component in ["etc", "etc/arkpkg", "etc/arkpkg/repos.d"] {
            let root = TempDir::new().unwrap();
            let link = root.path().join(component);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            fs::create_dir(root.path().join("storage")).unwrap();
            let target = PathBuf::from("../".repeat(Path::new(component).components().count() - 1))
                .join("storage");
            symlink(target, &link).unwrap();
            let repositories = RepositoryManager::new(root.path());

            repositories.add("main", "https://example.com/").unwrap();
            assert_eq!(repositories.list().unwrap()[0].url, "https://example.com");
            assert!(repositories.add("main", "https://example.org").is_err());
            assert!(
                repositories
                    .add("../escape", "https://example.com")
                    .is_err()
            );
            assert!(repositories.remove("../escape").is_err());
            repositories.remove("main").unwrap();
            assert!(repositories.list().unwrap().is_empty());
        }
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
