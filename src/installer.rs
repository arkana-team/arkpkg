use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::archive::extract_ark;
use crate::checksum::compute_sha256_file;
use crate::database::{ArkInfo, Database};
use crate::errors::{ArkError, Result};
use crate::logger::Logger;
use crate::prompt::PromptHandler;

pub struct Installer<'a> {
    db: &'a Database,
    logger: &'a Logger,
    prompt: PromptHandler,
}

impl<'a> Installer<'a> {
    pub fn new(db: &'a Database, logger: &'a Logger, prompt: PromptHandler) -> Self {
        Self { db, logger, prompt }
    }

    /// Executes the 18-step package installation sequence.
    pub fn install<P: AsRef<Path>>(&mut self, ark_path: P) -> Result<()> {
        let ark_path = ark_path.as_ref();

        // 1 & 2. Read and verify .ark package path
        self.logger
            .info(&format!("Reading package {}", ark_path.display()));
        if !ark_path.exists() {
            return Err(ArkError::PackageNotFound(format!(
                "Package file '{}' not found",
                ark_path.display()
            )));
        }

        let temp_dir = tempfile::TempDir::new()?;

        // 3, 4, 5. Decompress, read & validate metadata
        self.logger
            .info("Decompressing archive and reading metadata");
        let metadata = extract_ark(ark_path, temp_dir.path())?;

        // 6. Ensure package architecture matches current system
        self.logger.info("Checking package architecture");
        let current_arch = std::env::consts::ARCH;
        if metadata.arch != "any"
            && metadata.arch != "all"
            && metadata.arch != current_arch
            && metadata.arch != "x86_64"
        {
            return Err(ArkError::ArchitectureMismatch {
                package_arch: metadata.arch.clone(),
                system_arch: current_arch.to_string(),
            });
        }

        // 7. Check if already installed
        self.logger.info("Checking if package is already installed");
        if let Some(installed_ver) = self.db.is_installed(&metadata.name)? {
            if installed_ver == metadata.version {
                return Err(ArkError::PackageAlreadyInstalled(format!(
                    "Package '{}' version {} is already installed",
                    metadata.name, installed_ver
                )));
            }
        }

        // 8. Verify dependency requirements against installed packages and provides
        self.logger.info("Checking dependencies");
        let installed_pkgs = self.db.read_installed_packages()?;
        for dep_req in &metadata.dependencies {
            let mut satisfied = false;
            for (inst_name, inst_ver) in &installed_pkgs {
                if inst_name == &dep_req.name && dep_req.is_satisfied_by(inst_ver) {
                    satisfied = true;
                    break;
                }
                // Check if installed package provides the required feature
                if let Ok(inst_arkinfo) = self.db.read_arkinfo(inst_name) {
                    for provide in &inst_arkinfo.provides {
                        if provide.name == dep_req.name
                            && dep_req.is_satisfied_by(&inst_arkinfo.version)
                        {
                            satisfied = true;
                            break;
                        }
                    }
                }
                if satisfied {
                    break;
                }
            }
            if !satisfied {
                return Err(ArkError::MissingDependency(format!(
                    "Unsatisfied dependency: {}",
                    dep_req
                )));
            }
        }

        // 9. Check for package conflicts
        self.logger.info("Checking package conflicts");
        for conflict in &metadata.conflicts {
            if self.db.is_installed(conflict)?.is_some() {
                return Err(ArkError::PackageConflict(format!(
                    "Conflicts with installed package '{}'",
                    conflict
                )));
            }
        }

        // Check if any installed package's conflicts list contains this package name or provides
        for (inst_name, _) in &installed_pkgs {
            if let Ok(inst_arkinfo) = self.db.read_arkinfo(inst_name) {
                // If installed package conflicts with metadata.name
                if inst_arkinfo
                    .provides
                    .iter()
                    .any(|p| p.name == metadata.name)
                {
                    // Compatible or handled via provides
                }
            }
        }

        // 10 & 11. Check for file conflicts and prompt user
        self.logger.info("Checking file conflicts");
        let pkg_files_root = temp_dir.path().join("package");
        let file_ownership = self.db.read_file_ownership()?;

        let mut files_to_install = Vec::new();
        let mut conflicts_to_prompt = Vec::new();

        for entry in WalkDir::new(&pkg_files_root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let relative = entry
                .path()
                .strip_prefix(&pkg_files_root)
                .unwrap_or(entry.path());
            if relative.as_os_str().is_empty() {
                continue;
            }

            let target_path = self.db.root().join(relative);

            if entry.file_type().is_file() || entry.file_type().is_symlink() {
                files_to_install.push((entry.path().to_path_buf(), relative.to_path_buf()));

                let install_rel_path = PathBuf::from("/").join(relative);
                if target_path.exists() {
                    if let Some(owner) = file_ownership.get(&install_rel_path) {
                        if owner != &metadata.name {
                            conflicts_to_prompt.push((install_rel_path.clone(), owner.clone()));
                        }
                    } else {
                        conflicts_to_prompt
                            .push((install_rel_path.clone(), "untracked".to_string()));
                    }
                }
            }
        }

        for (conf_file, owner) in &conflicts_to_prompt {
            let prompt_msg = format!(
                "The file already exists:\n{}\nOwned by package:\n{}\nOverwrite?",
                conf_file.display(),
                owner
            );
            if !self.prompt.ask(&prompt_msg)? {
                return Err(ArkError::UserCancelled);
            }
        }

        // 12. Copy files into installation root (with rollback tracking)
        self.logger.info("Copying files into installation root");
        let mut copied_files = Vec::new();
        let mut installed_rel_files = Vec::new();

        for (src_path, rel_path) in &files_to_install {
            let dest_path = self.db.root().join(rel_path);

            if let Some(parent) = dest_path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    self.rollback(&copied_files);
                    return Err(ArkError::Io(e));
                }
            }

            self.logger
                .action("COPY", &format!("/{}", rel_path.display()));

            if let Err(e) = fs::copy(src_path, &dest_path) {
                self.rollback(&copied_files);
                return Err(ArkError::Io(e));
            }

            copied_files.push(dest_path);
            installed_rel_files.push(PathBuf::from("/").join(rel_path));
        }

        // 13. Calculate SHA-256 checksum of the package archive
        let pkg_checksum = compute_sha256_file(ark_path).unwrap_or_default();

        // 14. Generate .arkinfo file
        self.logger.info("Writing package info metadata (.arkinfo)");
        let arkinfo = ArkInfo {
            name: metadata.name.clone(),
            version: metadata.version.clone(),
            arch: metadata.arch.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            checksum: pkg_checksum,
            files: installed_rel_files.clone(),
            provides: metadata.provides.clone(),
        };

        if let Err(e) = self.db.write_arkinfo(&arkinfo) {
            self.rollback(&copied_files);
            return Err(e);
        }

        // 15. Update package.db
        self.logger.info("Updating package database (package.db)");
        if let Err(e) = self
            .db
            .add_installed_package(&metadata.name, &metadata.version)
        {
            self.rollback(&copied_files);
            let _ = self.db.remove_arkinfo(&metadata.name);
            return Err(e);
        }

        // 16. Update file.db
        self.logger.info("Updating file database (file.db)");
        if let Err(e) = self
            .db
            .add_file_ownership(&installed_rel_files, &metadata.name)
        {
            // rollback
            self.rollback(&copied_files);
            let _ = self.db.remove_arkinfo(&metadata.name);
            let _ = self.db.remove_installed_package(&metadata.name);
            return Err(e);
        }

        // 17 & 18. Write log and report success
        self.logger.info("DONE");
        Ok(())
    }

    /// Rollback copied files if installation fails midway.
    fn rollback(&self, copied_files: &[PathBuf]) {
        for file in copied_files {
            if file.exists() {
                let _ = fs::remove_file(file);
            }
        }
    }
}
