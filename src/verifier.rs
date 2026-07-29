use std::path::PathBuf;

use crate::database::Database;
use crate::errors::{ArkError, Result};

pub enum VerifyStatus {
    Ok(PathBuf),
    Modified(PathBuf),
    Missing(PathBuf),
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

        for file_rel in &arkinfo.files {
            let target_path = self
                .db
                .root()
                .join(file_rel.strip_prefix("/").unwrap_or(file_rel));

            if !target_path.exists() {
                println!("MISSING {}", file_rel.display());
                results.push(VerifyStatus::Missing(file_rel.clone()));
                failed = true;
            } else {
                // If file exists and is readable
                println!("OK      {}", file_rel.display());
                results.push(VerifyStatus::Ok(file_rel.clone()));
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
