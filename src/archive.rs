use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use tar::{Archive, Builder};

use crate::errors::{ArkError, Result};
use crate::metadata::Metadata;

/// Extracts an `.ark` (`tar.lz4`) package into a destination directory and parses its `ARKPKG` metadata.
pub fn extract_ark<P: AsRef<Path>, Q: AsRef<Path>>(
    archive_path: P,
    dest_dir: Q,
) -> Result<Metadata> {
    let archive_path = archive_path.as_ref();
    let dest_dir = dest_dir.as_ref();

    if !archive_path.exists() {
        return Err(ArkError::PackageNotFound(format!(
            "Package file '{}' not found",
            archive_path.display()
        )));
    }

    let file = File::open(archive_path)?;
    let lz4_decoder = FrameDecoder::new(file);
    let mut tar_archive = Archive::new(lz4_decoder);

    fs::create_dir_all(dest_dir)?;
    let entries = tar_archive
        .entries()
        .map_err(|error| ArkError::InvalidPackage(format!("Invalid package archive: {error}")))?;
    let mut seen_paths: HashMap<PathBuf, bool> = HashMap::new();
    let mut symlink_paths = HashSet::new();
    for entry in entries {
        let mut entry = entry.map_err(|error| {
            ArkError::InvalidPackage(format!("Invalid package archive: {error}"))
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
    use tempfile::TempDir;

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
        header.set_mode(0o644);
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
