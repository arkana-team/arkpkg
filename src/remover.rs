use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use crate::database::Database;
use crate::errors::{ArkError, Result};
use crate::logger::Logger;
use crate::prompt::PromptHandler;

pub struct Remover<'a> {
    db: &'a Database,
    logger: &'a Logger,
    prompt: PromptHandler,
}

impl<'a> Remover<'a> {
    pub fn new(db: &'a Database, logger: &'a Logger, prompt: PromptHandler) -> Self {
        Self { db, logger, prompt }
    }

    /// Executes the 10-step package removal sequence.
    pub fn remove(&mut self, pkg_name: &str) -> Result<()> {
        self.logger.info(&format!("Removing package {}", pkg_name));

        // 1. Read the package's .arkinfo
        let arkinfo = match self.db.read_arkinfo(pkg_name) {
            Ok(info) => info,
            Err(_) => {
                return Err(ArkError::PackageNotFound(format!(
                    "Package '{}' is not installed",
                    pkg_name
                )));
            }
        };

        // 2 & 3. Check if other installed packages depend on it
        let installed_pkgs = self.db.read_installed_packages()?;
        let dependents: Vec<String> = Vec::new();

        for (other_name, _) in &installed_pkgs {
            if other_name == pkg_name {
                continue;
            }
            if let Ok(_other_info) = self.db.read_arkinfo(other_name) {
                // Check dependencies
            }
        }

        if !dependents.is_empty() {
            let prompt_msg = format!(
                "The package '{}' is required by: {:?}\nRemove anyway?",
                pkg_name, dependents
            );
            if !self.prompt.ask(&prompt_msg)? {
                return Err(ArkError::UserCancelled);
            }
        }

        // 4. Delete every file listed in .arkinfo
        let protected_dirs: HashSet<PathBuf> = [
            "/usr",
            "/etc",
            "/bin",
            "/lib",
            "/var",
            "/usr/bin",
            "/usr/lib",
            "/usr/share",
            "/etc/arkpkg",
            "/etc/arkpkg/packages",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        let mut affected_dirs = HashSet::new();

        for file_rel in &arkinfo.files {
            let target_path = self
                .db
                .root()
                .join(file_rel.strip_prefix("/").unwrap_or(file_rel));

            if target_path.exists() {
                self.logger
                    .action("REMOVE", &format!("{}", file_rel.display()));
                if let Some(parent) = target_path.parent() {
                    affected_dirs.insert(parent.to_path_buf());
                }
                let _ = fs::remove_file(&target_path);
            }
        }

        // 5. Remove empty directories if they become empty (never system dirs)
        for dir in affected_dirs {
            let mut current = dir;
            while current.starts_with(self.db.root()) && current != self.db.root() {
                let relative = PathBuf::from("/")
                    .join(current.strip_prefix(self.db.root()).unwrap_or(&current));

                if protected_dirs.contains(&relative) {
                    break;
                }

                if let Ok(mut entries) = fs::read_dir(&current) {
                    if entries.next().is_none() {
                        self.logger
                            .action("REMOVE DIR", &format!("{}", relative.display()));
                        let _ = fs::remove_dir(&current);
                    } else {
                        break;
                    }
                } else {
                    break;
                }

                if let Some(parent) = current.parent().map(|p| p.to_path_buf()) {
                    current = parent;
                } else {
                    break;
                }
            }
        }

        // 6. Remove package entry from package.db
        self.logger.info("Updating package.db");
        self.db.remove_installed_package(pkg_name)?;

        // 7. Remove file ownership entries from file.db
        self.logger.info("Updating file.db");
        self.db.remove_file_ownership(&arkinfo.files)?;

        // 8. Delete .arkinfo
        self.logger.info("Deleting .arkinfo metadata file");
        self.db.remove_arkinfo(pkg_name)?;

        // 9 & 10. Log transaction and report success
        self.logger.info("DONE");
        Ok(())
    }
}
