use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use tar::{Archive, Builder};

use crate::errors::{ArkError, Result};
use crate::metadata::Metadata;

// Fixed per-package limits apply to local installation and repository resolution.
const MAX_ENTRY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const MAX_DECODED_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Bounds all decoded TAR data, including padding and extension records consumed
/// internally by `tar`. Exhaustion must be an error, not an apparent archive EOF.
pub(crate) struct ArchiveReader<R> {
    inner: io::Take<R>,
}

impl<R: Read> ArchiveReader<R> {
    pub(crate) fn new(reader: R, limit: u64) -> Self {
        Self {
            inner: reader.take(limit),
        }
    }
}

impl<R: Read> Read for ArchiveReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !buffer.is_empty() && self.inner.limit() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Package exceeds the decoded archive byte limit",
            ));
        }
        self.inner.read(buffer)
    }
}

/// Extracts an `.ark` (`tar.lz4`) package into a destination directory and parses its `ARKPKG` metadata.
/// Entries are limited to 2 GiB and total decoded TAR data to 4 GiB, including headers and padding.
pub fn extract_ark<P: AsRef<Path>, Q: AsRef<Path>>(
    archive_path: P,
    dest_dir: Q,
) -> Result<Metadata> {
    extract_ark_with_limits(
        archive_path.as_ref(),
        dest_dir.as_ref(),
        MAX_ENTRY_BYTES,
        MAX_DECODED_ARCHIVE_BYTES,
    )
}

fn extract_ark_with_limits(
    archive_path: &Path,
    dest_dir: &Path,
    max_entry_bytes: u64,
    max_decoded_bytes: u64,
) -> Result<Metadata> {
    if !archive_path.exists() {
        return Err(ArkError::PackageNotFound(format!(
            "Package file '{}' not found",
            archive_path.display()
        )));
    }

    let file = File::open(archive_path)?;
    let lz4_decoder = FrameDecoder::new(file);
    let mut tar_archive = Archive::new(ArchiveReader::new(lz4_decoder, max_decoded_bytes));

    fs::create_dir_all(dest_dir)?;
    let entries = tar_archive
        .entries()
        .map_err(|error| ArkError::InvalidPackage(format!("Invalid package archive: {error}")))?;
    let mut seen_paths: HashMap<PathBuf, bool> = HashMap::new();
    let mut symlink_paths = HashSet::new();
    let mut declared_bytes = 0u64;
    for entry in entries {
        let mut entry = entry.map_err(|error| {
            ArkError::InvalidPackage(format!("Invalid package archive: {error}"))
        })?;
        // Use the effective size: PAX extensions can override the raw header.
        // tar bounds regular-file reads/writes to this size; sparse files remain
        // disallowed by the entry-type checks below.
        let size = entry.size();
        if size > max_entry_bytes {
            return Err(ArkError::InvalidPackage(format!(
                "Archive entry exceeds the {max_entry_bytes} byte entry limit"
            )));
        }
        declared_bytes = declared_bytes
            .checked_add(size)
            .filter(|total| *total <= max_decoded_bytes)
            .ok_or_else(|| {
                ArkError::InvalidPackage(format!(
                    "Archive entries exceed the {max_decoded_bytes} byte total limit"
                ))
            })?;
        let path = entry
            .path()
            .map_err(|error| ArkError::InvalidPackage(format!("Invalid archive path: {error}")))?
            .into_owned();
        let safe_relative_path = !path.as_os_str().is_empty()
            && !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        let allowed_root = path == Path::new("ARKPKG") || path.starts_with("package");
        if !safe_relative_path || !allowed_root {
            return Err(ArkError::InvalidPackage(format!(
                "Unsafe or unexpected archive path '{}'",
                path.display()
            )));
        }
        if symlink_paths
            .iter()
            .any(|symlink: &PathBuf| path != *symlink && path.starts_with(symlink))
        {
            return Err(ArkError::InvalidPackage(format!(
                "Archive entry '{}' descends through a symlink",
                path.display()
            )));
        }
        let entry_type = entry.header().entry_type();
        if let Some(previous_was_directory) = seen_paths.get(&path) {
            if !*previous_was_directory || !entry_type.is_dir() {
                return Err(ArkError::InvalidPackage(format!(
                    "Duplicate archive entry at '{}'",
                    path.display()
                )));
            }
        } else {
            seen_paths.insert(path.clone(), entry_type.is_dir());
        }
        if !(entry_type.is_file() || entry_type.is_dir() || entry_type.is_symlink()) {
            return Err(ArkError::InvalidPackage(format!(
                "Unsupported archive entry type at '{}'",
                path.display()
            )));
        }
        if path == Path::new("ARKPKG") && !entry_type.is_file() {
            return Err(ArkError::InvalidPackage(
                "Archive metadata entry must be a regular file".into(),
            ));
        }
        if path == Path::new("package") && !entry_type.is_dir() {
            return Err(ArkError::InvalidPackage(
                "Archive package root must be a directory".into(),
            ));
        }
        if entry_type.is_symlink() {
            let target = entry
                .link_name()
                .map_err(|error| {
                    ArkError::InvalidPackage(format!("Invalid symlink target: {error}"))
                })?
                .ok_or_else(|| {
                    ArkError::InvalidPackage("Archive symlink is missing its target".into())
                })?;
            if !safe_archive_symlink_target(&path, &target) {
                return Err(ArkError::InvalidPackage(format!(
                    "Symlink '{}' points outside package contents",
                    path.display()
                )));
            }
            symlink_paths.insert(path.clone());
        }
        let extracted = entry.unpack_in(dest_dir).map_err(|error| {
            ArkError::InvalidPackage(format!("Failed to extract archive: {error}"))
        })?;
        if !extracted {
            return Err(ArkError::InvalidPackage(format!(
                "Archive entry '{}' escaped the extraction directory",
                path.display()
            )));
        }
    }

    let metadata_path = dest_dir.join("ARKPKG");
    if !metadata_path.is_file() {
        return Err(ArkError::InvalidPackage(
            "Archive missing root 'ARKPKG' metadata file".into(),
        ));
    }

    let pkg_dir = dest_dir.join("package");
    if !pkg_dir.is_dir() {
        return Err(ArkError::InvalidPackage(
            "Archive missing 'package/' directory".into(),
        ));
    }

    let meta_content = fs::read_to_string(metadata_path)?;
    Metadata::parse(&meta_content)
}

fn safe_archive_symlink_target(link_path: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return target
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)));
    }
    let relative_link = match link_path.strip_prefix("package") {
        Ok(path) => path,
        Err(_) => return false,
    };
    let mut depth = relative_link
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

/// Packs a package directory (containing `ARKPKG` and `package/`) into an `.ark` (`tar.lz4`) package.
pub fn create_ark<P: AsRef<Path>, Q: AsRef<Path>>(source_dir: P, output_ark_path: Q) -> Result<()> {
    let source_dir = source_dir.as_ref();
    let output_ark_path = output_ark_path.as_ref();

    let metadata_path = source_dir.join("ARKPKG");
    if !metadata_path.exists() {
        return Err(ArkError::InvalidPackage(format!(
            "Directory '{}' missing 'ARKPKG' file",
            source_dir.display()
        )));
    }

    let pkg_dir = source_dir.join("package");
    if !pkg_dir.exists() {
        return Err(ArkError::InvalidPackage(format!(
            "Directory '{}' missing 'package/' folder",
            source_dir.display()
        )));
    }

    // Validate metadata before packing
    let meta_content = fs::read_to_string(&metadata_path)?;
    let _metadata = Metadata::parse(&meta_content)?;

    if let Some(parent) = output_ark_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let output_file = File::create(output_ark_path)?;
    let lz4_encoder = FrameEncoder::new(output_file);
    let mut tar_builder = Builder::new(lz4_encoder);
    tar_builder.follow_symlinks(false);

    // Add ARKPKG file
    tar_builder
        .append_path_with_name(&metadata_path, "ARKPKG")
        .map_err(|e| ArkError::Generic(format!("Failed to pack ARKPKG: {}", e)))?;

    // Add package/ directory contents recursively
    tar_builder
        .append_dir_all("package", &pkg_dir)
        .map_err(|e| ArkError::Generic(format!("Failed to pack package/ directory: {}", e)))?;

    let lz4_encoder = tar_builder
        .into_inner()
        .map_err(|e| ArkError::Generic(format!("Failed to finish TAR archive: {}", e)))?;

    lz4_encoder
        .finish()
        .map_err(|e| ArkError::Generic(format!("Failed to finish LZ4 compression: {}", e)))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{NamedTempFile, TempDir};

    fn test_archive(build: impl FnOnce(&mut Builder<FrameEncoder<File>>)) -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        let mut archive = Builder::new(FrameEncoder::new(file.reopen().unwrap()));
        build(&mut archive);
        archive.into_inner().unwrap().finish().unwrap();
        file
    }

    #[test]
    fn rejects_oversized_declared_entry_before_creating_file() {
        let archive = test_archive(|archive| {
            let mut header = tar::Header::new_gnu();
            header.set_path("package/huge").unwrap();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(MAX_ENTRY_BYTES + 1);
            header.set_mode(0o644);
            header.set_cksum();
            // A header alone must be rejected without reading its claimed body.
            archive.append(&header, io::empty()).unwrap();
        });
        let dest = TempDir::new().unwrap();
        let error = extract_ark(archive.path(), dest.path()).unwrap_err();
        assert!(matches!(error, ArkError::InvalidPackage(_)));
        assert!(error.to_string().contains("entry limit"));
        assert!(!dest.path().join("package/huge").exists());
    }

    #[test]
    fn rejects_cumulative_entry_sizes_before_writing_offending_file() {
        let archive = test_archive(|archive| {
            append_entry(
                archive,
                "package/first",
                tar::EntryType::Regular,
                &[0; 4096],
            );
            append_entry(
                archive,
                "package/second",
                tar::EntryType::Regular,
                &[0; 4097],
            );
        });
        let dest = TempDir::new().unwrap();
        let error = extract_ark_with_limits(archive.path(), dest.path(), 8192, 8192).unwrap_err();
        assert!(error.to_string().contains("total limit"));
        assert_eq!(
            fs::metadata(dest.path().join("package/first"))
                .unwrap()
                .len(),
            4096
        );
        assert!(!dest.path().join("package/second").exists());
    }

    #[test]
    fn stops_expanded_writes_at_decoded_budget() {
        let archive = test_archive(|archive| {
            append_entry(archive, "package/bomb", tar::EntryType::Regular, &[0; 4096]);
        });
        assert!(archive.as_file().metadata().unwrap().len() < 512);
        let dest = TempDir::new().unwrap();
        let error = extract_ark_with_limits(archive.path(), dest.path(), 4096, 4096).unwrap_err();
        assert!(matches!(error, ArkError::InvalidPackage(_)));
        // The TAR header also consumes the decoded budget.
        assert_eq!(
            fs::metadata(dest.path().join("package/bomb"))
                .unwrap()
                .len(),
            4096 - 512
        );
    }

    #[test]
    fn counts_tar_extensions_against_decoded_budget() {
        let archive = test_archive(|archive| {
            append_entry(
                archive,
                "././@LongLink",
                tar::EntryType::GNULongName,
                &[b'a'; 4096],
            );
            append_entry(archive, "package/file", tar::EntryType::Regular, b"x");
        });
        let dest = TempDir::new().unwrap();
        let error = extract_ark_with_limits(archive.path(), dest.path(), 4096, 1024).unwrap_err();
        assert!(error.to_string().contains("decoded archive byte limit"));
        assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    #[test]
    fn checks_pax_size_override_instead_of_raw_header_size() {
        let archive = test_archive(|archive| {
            archive
                .append_pax_extensions([("size", b"4096".as_slice())])
                .unwrap();
            append_entry(archive, "package/file", tar::EntryType::Regular, b"x");
        });
        let dest = TempDir::new().unwrap();
        let error = extract_ark_with_limits(archive.path(), dest.path(), 1024, 8192).unwrap_err();
        assert!(error.to_string().contains("entry limit"));
        assert!(!dest.path().join("package/file").exists());
    }

    #[test]
    fn accepts_entry_at_limit_and_archive_at_decoded_budget() {
        let archive = test_archive(|archive| {
            append_entry(
                archive,
                "ARKPKG",
                tar::EntryType::Regular,
                b"Name: bounded\nDescription: test\nVersion: 1.0\nArch: any\n",
            );
            append_entry(archive, "package", tar::EntryType::Directory, b"");
            append_entry(
                archive,
                "package/file",
                tar::EntryType::Regular,
                &[b'x'; 512],
            );
        });
        let dest = TempDir::new().unwrap();
        // Metadata header/body, directory header, payload header/body and the
        // first end-of-archive block: exactly six TAR blocks are consumed.
        let metadata = extract_ark_with_limits(archive.path(), dest.path(), 512, 6 * 512).unwrap();
        assert_eq!(metadata.name, "bounded");
        assert_eq!(
            fs::read(dest.path().join("package/file")).unwrap(),
            [b'x'; 512]
        );

        // Exhaustion on a header boundary must not look like successful EOF.
        let dest = TempDir::new().unwrap();
        let error = extract_ark_with_limits(archive.path(), dest.path(), 512, 5 * 512).unwrap_err();
        assert!(error.to_string().contains("decoded archive byte limit"));
    }

    fn append_entry(
        archive: &mut Builder<FrameEncoder<File>>,
        path: &str,
        entry_type: tar::EntryType,
        content: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(entry_type);
        header.set_size(content.len() as u64);
        header.set_mode(if entry_type.is_dir() { 0o755 } else { 0o644 });
        header.set_cksum();
        archive.append(&header, content).unwrap();
    }

    #[test]
    fn test_create_and_extract_ark() {
        let src_dir = TempDir::new().unwrap();
        let ark_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let arkpkg_content = r#"
Name: hello
Description: Test package
Version: 1.0.0
Arch: x86_64
"#;
        fs::write(src_dir.path().join("ARKPKG"), arkpkg_content).unwrap();

        let pkg_bin_dir = src_dir.path().join("package/usr/bin");
        fs::create_dir_all(&pkg_bin_dir).unwrap();
        fs::write(pkg_bin_dir.join("hello"), b"#!/bin/sh\necho hello").unwrap();

        let out_ark = ark_dir.path().join("hello-1.0.0-x86_64.ark");

        create_ark(src_dir.path(), &out_ark).unwrap();
        assert!(out_ark.exists());

        let meta = extract_ark(&out_ark, dest_dir.path()).unwrap();
        assert_eq!(meta.name, "hello");
        assert_eq!(meta.version.as_str(), "1.0.0");
        assert!(dest_dir.path().join("package/usr/bin/hello").exists());
    }

    #[test]
    fn rejects_archive_paths_that_escape_the_extraction_root() {
        let archive_dir = TempDir::new().unwrap();
        let extraction_dir = TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("unsafe.ark");

        let output = File::create(&archive_path).unwrap();
        let encoder = FrameEncoder::new(output);
        let mut tar_builder = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_path("safe-entry").unwrap();
        header.as_mut_bytes()[..11].copy_from_slice(b"../outside\0");
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append(&header, &b"x"[..]).unwrap();
        let encoder = tar_builder.into_inner().unwrap();
        encoder.finish().unwrap();

        assert!(extract_ark(&archive_path, extraction_dir.path()).is_err());
        assert!(
            !extraction_dir
                .path()
                .parent()
                .unwrap()
                .join("outside")
                .exists()
        );
    }

    #[test]
    fn rejects_duplicate_payload_paths() {
        let archive_dir = TempDir::new().unwrap();
        let extraction_dir = TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("duplicate-payload.ark");
        let output = File::create(&archive_path).unwrap();
        let mut archive = Builder::new(FrameEncoder::new(output));
        let metadata = b"Name: duplicate\nDescription: test\nVersion: 1.0\nArch: any\n";
        append_entry(&mut archive, "ARKPKG", tar::EntryType::Regular, metadata);
        append_entry(&mut archive, "package", tar::EntryType::Directory, b"");
        append_entry(
            &mut archive,
            "package/usr/bin/tool",
            tar::EntryType::Regular,
            b"first payload",
        );
        append_entry(
            &mut archive,
            "package/usr/bin/tool",
            tar::EntryType::Regular,
            b"replacement payload",
        );
        archive.into_inner().unwrap().finish().unwrap();

        assert!(extract_ark(&archive_path, extraction_dir.path()).is_err());
    }

    #[test]
    fn rejects_duplicate_metadata_entries() {
        let archive_dir = TempDir::new().unwrap();
        let extraction_dir = TempDir::new().unwrap();
        let archive_path = archive_dir.path().join("duplicate-metadata.ark");
        let output = File::create(&archive_path).unwrap();
        let mut archive = Builder::new(FrameEncoder::new(output));
        let metadata = b"Name: duplicate\nDescription: test\nVersion: 1.0\nArch: any\n";
        append_entry(&mut archive, "ARKPKG", tar::EntryType::Regular, metadata);
        append_entry(&mut archive, "ARKPKG", tar::EntryType::Regular, metadata);
        append_entry(&mut archive, "package", tar::EntryType::Directory, b"");
        archive.into_inner().unwrap().finish().unwrap();

        assert!(extract_ark(&archive_path, extraction_dir.path()).is_err());
    }
}
