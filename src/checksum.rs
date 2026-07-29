use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Result as IoResult};
use std::path::Path;

/// Computes the SHA-256 hash of a file at the given path.
pub fn compute_sha256_file<P: AsRef<Path>>(path: P) -> IoResult<String> {
    let mut file = File::open(path)?;
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
}
