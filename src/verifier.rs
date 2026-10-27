use std::fs;
use std::path::PathBuf;

use crate::checksum::{compute_integrity_path, compute_sha256_path, typed_file_digest};
use crate::database::Database;
use crate::errors::{ArkError, Result};

pub enum VerifyStatus {
    Ok(PathBuf),
    Modified(PathBuf),
    Missing(PathBuf),
    Unverified(PathBuf),
}

pub struct Verifier<'a> {
    db: &'a Database,
}

impl<'a> Verifier<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Verifies all installed files of a package.
    pub fn verify(&self, pkg_name: &str) -> Result<Vec<VerifyStatus>> {
        let arkinfo = self.db.read_arkinfo(pkg_name).map_err(|_| {
            ArkError::PackageNotFound(format!("Package '{}' is not installed", pkg_name))
        })?;

        let mut results = Vec::new();
        let mut failed = false;
        let file_hashes: std::collections::HashMap<_, _> =
            arkinfo.file_hashes.iter().cloned().collect();

        for (_, backup_rel) in &arkinfo.preserved_files {
            let backup_path = self
                .db
                .root()
                .join(backup_rel.strip_prefix("/").unwrap_or(backup_rel));
            match fs::symlink_metadata(&backup_path) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    println!("OK      preserved original {}", backup_rel.display());
                    results.push(VerifyStatus::Ok(backup_rel.clone()));
                }
                Ok(_) => {
                    println!("MISSING preserved original {}", backup_rel.display());
                    results.push(VerifyStatus::Missing(backup_rel.clone()));
                    failed = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    println!("MISSING preserved original {}", backup_rel.display());
                    results.push(VerifyStatus::Missing(backup_rel.clone()));
                    failed = true;
                }
                Err(error) => return Err(ArkError::Io(error)),
            }
        }

        for file_rel in &arkinfo.files {
            let target_path = self
                .db
                .root()
                .join(file_rel.strip_prefix("/").unwrap_or(file_rel));

            match fs::symlink_metadata(&target_path) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    if let Some(expected) = file_hashes.get(file_rel) {
                        let typed = typed_file_digest(expected).is_some();
                        let actual = if typed {
                            compute_integrity_path(&target_path)?
                        } else {
                            compute_sha256_path(&target_path)?
                        };
                        if !typed && actual.eq_ignore_ascii_case(expected) {
                            println!(
                                "UNVERIFIED {} (legacy metadata lacks object type)",
                                file_rel.display()
                            );
                            results.push(VerifyStatus::Unverified(file_rel.clone()));
                        } else if actual.eq_ignore_ascii_case(expected) {
                            println!("OK      {}", file_rel.display());
                            results.push(VerifyStatus::Ok(file_rel.clone()));
                        } else {
                            println!("MODIFIED {}", file_rel.display());
                            results.push(VerifyStatus::Modified(file_rel.clone()));
                            failed = true;
                        }
                    } else {
                        println!("UNVERIFIED {} (legacy metadata)", file_rel.display());
                        results.push(VerifyStatus::Unverified(file_rel.clone()));
                        failed = true;
                    }
                }
                Ok(_) => {
                    println!("MODIFIED {}", file_rel.display());
                    results.push(VerifyStatus::Modified(file_rel.clone()));
                    failed = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    println!("MISSING {}", file_rel.display());
                    results.push(VerifyStatus::Missing(file_rel.clone()));
                    failed = true;
                }
                Err(error) => return Err(ArkError::Io(error)),
            }
        }

        if failed {
            Err(ArkError::VerificationFailed(format!(
                "Verification failed for package '{}'",
                pkg_name
            )))
        } else {
            Ok(results)
        }
    }
}
