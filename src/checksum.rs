use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Result as IoResult};
use std::path::Path;

/// Computes the SHA-256 hash of a file at the given path.
pub fn compute_sha256_file<P: AsRef<Path>>(path: P) -> IoResult<String> {
    compute_sha256_reader(File::open(path)?)
}

/// Hash an already opened file without resolving its pathname again.
pub(crate) fn compute_sha256_reader(mut file: impl Read) -> IoResult<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Computes the SHA-256 hash of raw byte slice.
pub fn compute_sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Computes the SHA-256 hash of a regular file or of a symlink's stored target.
pub fn compute_sha256_path<P: AsRef<Path>>(path: P) -> IoResult<String> {
    Ok(compute_path_digest(path.as_ref())?.1)
}

/// Records the object type alongside its digest for installed-file verification.
/// The tag is part of the compared value; raw archive SHA-256 values stay unchanged.
pub(crate) fn compute_integrity_path<P: AsRef<Path>>(path: P) -> IoResult<String> {
    let (kind, digest) = compute_path_digest(path.as_ref())?;
    Ok(format!("{kind}{digest}"))
}

/// Returns the digest only for a recognized typed integrity record.
pub(crate) fn typed_file_digest(value: &str) -> Option<&str> {
    value
        .strip_prefix("file-sha256=")
        .or_else(|| value.strip_prefix("symlink-sha256="))
}

fn compute_path_digest(path: &Path) -> IoResult<(&'static str, String)> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return Ok((
                "symlink-sha256=",
                compute_sha256_bytes(target.as_os_str().as_bytes()),
            ));
        }
        #[cfg(not(unix))]
        {
            return Ok((
                "symlink-sha256=",
                compute_sha256_bytes(target.to_string_lossy().as_bytes()),
            ));
        }
    }
    if metadata.is_file() {
        return Ok(("file-sha256=", compute_sha256_file(path)?));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "checksum path is neither a regular file nor a symlink",
    ))
}

/// Verifies whether the SHA-256 hash of a file matches the expected hash string.
pub fn verify_file_sha256<P: AsRef<Path>>(path: P, expected_hash: &str) -> IoResult<bool> {
    let actual_hash = compute_sha256_file(path)?;
    Ok(actual_hash.eq_ignore_ascii_case(expected_hash.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_checksum_computation() {
        let data = b"hello arkanaOS package manager";
        let expected = compute_sha256_bytes(data);

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(data).unwrap();

        let file_hash = compute_sha256_file(tmp.path()).unwrap();
        assert_eq!(expected, file_hash);
        assert!(verify_file_sha256(tmp.path(), &expected).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn hashes_symlink_target_without_following_it() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::TempDir::new().unwrap();
        let link = directory.path().join("command");
        symlink("first-target", &link).unwrap();
        let first = compute_sha256_path(&link).unwrap();
        fs::remove_file(&link).unwrap();
        symlink("second-target", &link).unwrap();
        let second = compute_sha256_path(&link).unwrap();

        assert_ne!(first, second);
        assert_eq!(first, compute_sha256_bytes(b"first-target"));
    }
}
