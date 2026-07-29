use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use std::fs::{self, File};
use std::path::Path;
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
    tar_archive
        .unpack(dest_dir)
        .map_err(|e| ArkError::InvalidPackage(format!("Failed to extract archive: {}", e)))?;

    let metadata_path = dest_dir.join("ARKPKG");
    if !metadata_path.exists() {
        return Err(ArkError::InvalidPackage(
            "Archive missing root 'ARKPKG' metadata file".into(),
        ));
    }

    let pkg_dir = dest_dir.join("package");
    if !pkg_dir.exists() {
        return Err(ArkError::InvalidPackage(
            "Archive missing 'package/' directory".into(),
        ));
    }

    let meta_content = fs::read_to_string(metadata_path)?;
    Metadata::parse(&meta_content)
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

    if let Some(parent) = output_ark_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let output_file = File::create(output_ark_path)?;
    let lz4_encoder = FrameEncoder::new(output_file);
    let mut tar_builder = Builder::new(lz4_encoder);

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
}
