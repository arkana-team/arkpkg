use tempfile::NamedTempFile;

use crate::archive::extract_ark;
use crate::database::Database;
use crate::errors::{ArkError, Result};
use crate::metadata::{Metadata, ProvideItem, validate_package_name};
use crate::repository::{RepositoryCatalog, read_archive_metadata};
use crate::version::{DependencyReq, Version};

const MAX_DEPENDENCY_DEPTH: usize = 128;
const MAX_INSTALL_PLAN_PACKAGES: usize = 256;
const MAX_INSTALL_PLAN_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct ResolvedPackage {
    pub metadata: Metadata,
    pub archive: NamedTempFile,
}

/// Downloads a repository package and its missing dependencies in installation order.
pub fn resolve_install_plan(
    database: &Database,
    catalog: &RepositoryCatalog,
    package_name: &str,
) -> Result<Vec<ResolvedPackage>> {
    resolve_install_plan_with(database, package_name, |requirement| {
        catalog.download_package_matching(requirement)
    })
}

/// Fully validates a confirmed plan before installing its first package.
pub fn validate_install_plan(plan: &[ResolvedPackage]) -> Result<()> {
    for package in plan {
        // Release each extraction before validating the next package; only one
        // expanded archive is live, subject to the existing per-archive limits.
        let extraction = crate::temp::directory()?;
        extract_ark(package.archive.path(), extraction.path())?;
    }
    Ok(())
}

fn resolve_install_plan_with<F>(
    database: &Database,
    package_name: &str,
    download: F,
) -> Result<Vec<ResolvedPackage>>
where
    F: FnMut(&DependencyReq) -> Result<NamedTempFile>,
{
    validate_package_name(package_name)?;
    let root_requirement = DependencyReq::parse(package_name)?;
    let mut resolver = DependencyResolver {
        database,
        download,
        installed: read_installed(database)?,
        completed: Vec::new(),
        visiting: Vec::new(),
        downloaded_bytes: 0,
    };
    resolver.resolve(&root_requirement, true)?;
    Ok(resolver.completed)
}

struct InstalledPackage {
    name: String,
    version: Version,
    provides: Vec<ProvideItem>,
}

struct DependencyResolver<'a, F> {
    database: &'a Database,
    download: F,
    installed: Vec<InstalledPackage>,
    completed: Vec<ResolvedPackage>,
    visiting: Vec<Metadata>,
    downloaded_bytes: u64,
}

impl<F> DependencyResolver<'_, F>
where
    F: FnMut(&DependencyReq) -> Result<NamedTempFile>,
{
    fn resolve(&mut self, requirement: &DependencyReq, root: bool) -> Result<()> {
        if !root && self.installed_satisfies(requirement) {
            return Ok(());
        }

        if self
            .completed
            .iter()
            .any(|package| metadata_satisfies(&package.metadata, requirement))
        {
            return Ok(());
        }

        if let Some(index) = self
            .visiting
            .iter()
            .position(|metadata| metadata_satisfies(metadata, requirement))
        {
            let mut cycle: Vec<&str> = self.visiting[index..]
                .iter()
                .map(|metadata| metadata.name.as_str())
                .collect();
            cycle.push(self.visiting[index].name.as_str());
            return Err(ArkError::InvalidPackage(format!(
                "Package dependency cycle: {}",
                cycle.join(" -> ")
            )));
        }

        if let Some(existing) = self.completed.iter().find(|package| {
            package
                .metadata
                .name
                .eq_ignore_ascii_case(&requirement.name)
        }) {
            return Err(ArkError::PackageConflict(format!(
                "Planned package '{}' version {} does not satisfy dependency '{}'",
                existing.metadata.name, existing.metadata.version, requirement
            )));
        }
        if let Some(existing) = self
            .visiting
            .iter()
            .find(|metadata| metadata.name.eq_ignore_ascii_case(&requirement.name))
        {
            return Err(ArkError::PackageConflict(format!(
                "Package '{}' is required at incompatible versions while resolving '{}'",
                existing.name, requirement
            )));
        }

        if self.visiting.len() >= MAX_DEPENDENCY_DEPTH {
            return Err(ArkError::InvalidPackage(format!(
                "Package dependency depth exceeds the limit of {MAX_DEPENDENCY_DEPTH}"
            )));
        }
        if self.completed.len() + self.visiting.len() >= MAX_INSTALL_PLAN_PACKAGES {
            return Err(ArkError::InvalidPackage(format!(
                "Package dependency plan exceeds the limit of {MAX_INSTALL_PLAN_PACKAGES} packages"
            )));
        }

        let archive = (self.download)(requirement)?;
        let archive_size = archive.as_file().metadata()?.len();
        self.downloaded_bytes = self
            .downloaded_bytes
            .checked_add(archive_size)
            .filter(|size| *size <= MAX_INSTALL_PLAN_DOWNLOAD_BYTES)
            .ok_or_else(|| {
                ArkError::InvalidPackage(format!(
                    "Dependency downloads exceed the {} byte install-plan limit",
                    MAX_INSTALL_PLAN_DOWNLOAD_BYTES
                ))
            })?;

        // Planning needs only bounded metadata; full validation follows confirmation.
        let metadata = read_archive_metadata(archive.path())?;
        if !metadata_satisfies(&metadata, requirement) {
            return Err(ArkError::VerificationFailed(format!(
                "Downloaded package '{}' does not satisfy '{}'",
                metadata.name, requirement
            )));
        }

        if root {
            if let Some(installed_version) = self.database.is_installed(&metadata.name)? {
                if installed_version == metadata.version {
                    return Err(ArkError::PackageAlreadyInstalled(format!(
                        "Package '{}' version {} is already installed",
                        metadata.name, installed_version
                    )));
                }
                return Err(ArkError::PackageConflict(format!(
                    "Package '{}' is already installed at version {}; remove it before installing {}",
                    metadata.name, installed_version, metadata.version
                )));
            }
        } else if let Some(installed_version) = self.database.is_installed(&metadata.name)? {
            return Err(ArkError::PackageConflict(format!(
                "Installed package '{}' version {} does not satisfy '{}'",
                metadata.name, installed_version, requirement
            )));
        }

        let dependencies = metadata.dependencies.clone();
        self.visiting.push(metadata.clone());
        for dependency in &dependencies {
            self.resolve(dependency, false)?;
        }
        self.visiting.pop();

        self.completed.push(ResolvedPackage { metadata, archive });
        Ok(())
    }

    fn installed_satisfies(&self, requirement: &DependencyReq) -> bool {
        self.installed.iter().any(|package| {
            (package.name.eq_ignore_ascii_case(&requirement.name)
                || package
                    .provides
                    .iter()
                    .any(|provide| provide.name.eq_ignore_ascii_case(&requirement.name)))
                && requirement.is_satisfied_by(&package.version)
        })
    }
}

fn metadata_satisfies(metadata: &Metadata, requirement: &DependencyReq) -> bool {
    (metadata.name.eq_ignore_ascii_case(&requirement.name)
        || metadata
            .provides
            .iter()
            .any(|provide| provide.name.eq_ignore_ascii_case(&requirement.name)))
        && requirement.is_satisfied_by(&metadata.version)
}

fn read_installed(database: &Database) -> Result<Vec<InstalledPackage>> {
    Ok(database
        .read_installed_packages()?
        .into_iter()
        .map(|(name, version)| {
            let provides = database
                .read_arkinfo(&name)
                .map(|info| info.provides)
                .unwrap_or_default();
            InstalledPackage {
                name,
                version,
                provides,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write;

    use crate::archive::create_ark;
    use crate::installer::Installer;
    use crate::logger::Logger;
    use crate::prompt::PromptHandler;
    use tempfile::TempDir;

    #[test]
    fn metadata_satisfaction_checks_names_provides_and_version_constraints() {
        let metadata = Metadata::parse(
            "Name: shell\nDescription: test shell\nVersion: 2.1\nArch: any\nProvides:\nsh=/bin/sh\n",
        )
        .unwrap();
        assert!(metadata_satisfies(
            &metadata,
            &DependencyReq::parse("shell>=2.0").unwrap()
        ));
        assert!(metadata_satisfies(
            &metadata,
            &DependencyReq::parse("sh>=2.0").unwrap()
        ));
        assert!(!metadata_satisfies(
            &metadata,
            &DependencyReq::parse("sh>=3.0").unwrap()
        ));
    }

    #[test]
    fn dependency_plan_is_topological() {
        let temp = TempDir::new().unwrap();
        let database = Database::new(temp.path());
        database.init().unwrap();
        let packages = [("app", "lib>=1.0"), ("lib", "base"), ("base", "")];
        let mut archives = HashMap::new();
        for (name, dependency) in packages {
            let source = temp.path().join(format!("source-{name}"));
            fs::create_dir_all(source.join("package/usr/share")).unwrap();
            let dep_line = if dependency.is_empty() {
                String::new()
            } else {
                format!("Dependencies:\n{dependency}\n")
            };
            fs::write(
                source.join("ARKPKG"),
                format!(
                    "Name: {name}\nDescription: test package\nVersion: 1.0\nArch: any\n{dep_line}"
                ),
            )
            .unwrap();
            fs::write(source.join("package/usr/share/file"), name).unwrap();
            let archive_path = temp.path().join(format!("{name}.ark"));
            create_ark(&source, &archive_path).unwrap();
            archives.insert(name.to_string(), archive_path);
        }

        let plan = resolve_install_plan_with(&database, "app", |requirement| {
            let path = archives.get(&requirement.name).unwrap();
            let mut temporary = NamedTempFile::new().unwrap();
            std::io::copy(&mut fs::File::open(path).unwrap(), &mut temporary).unwrap();
            temporary.flush().unwrap();
            Ok(temporary)
        })
        .unwrap();
        let names: Vec<_> = plan
            .iter()
            .map(|package| package.metadata.name.as_str())
            .collect();
        assert_eq!(names, ["base", "lib", "app"]);
        validate_install_plan(&plan).unwrap();
    }

    #[test]
    fn dependency_planning_reads_metadata_without_expanding_payloads() {
        let temp = TempDir::new().unwrap();
        let database = Database::new(temp.path());
        database.init().unwrap();

        let plan = resolve_install_plan_with(&database, "app", |requirement| {
            let dependencies = if requirement.name == "app" {
                "Dependencies:\nlib\n"
            } else {
                assert_eq!(requirement.name, "lib");
                ""
            };
            let metadata = format!(
                "Name: {}\nDescription: test package\nVersion: 1.0\nArch: any\n{dependencies}",
                requirement.name
            );
            let mut file = NamedTempFile::new().unwrap();
            let mut archive =
                tar::Builder::new(lz4_flex::frame::FrameEncoder::new(file.as_file_mut()));
            let mut header = tar::Header::new_gnu();
            header.set_size(metadata.len() as u64);
            header.set_mode(0o644);
            archive
                .append_data(&mut header, "ARKPKG", metadata.as_bytes())
                .unwrap();

            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            archive
                .append_data(&mut header, "package", std::io::empty())
                .unwrap();
            if requirement.name == "app" {
                // Omit the oversized payload body to keep the fixture small.
                // Planning must finish without inspecting or expanding it.
                header.set_path("package/huge").unwrap();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(2 * 1024 * 1024 * 1024 + 1);
                header.set_cksum();
                archive.append(&header, std::io::empty()).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
            Ok(file)
        })
        .unwrap();

        let names: Vec<_> = plan
            .iter()
            .map(|package| package.metadata.name.as_str())
            .collect();
        assert_eq!(names, ["lib", "app"]);
        assert!(database.read_installed_packages().unwrap().is_empty());

        // Even with a valid dependency first, reject the invalid application
        // before installing any package from the plan.
        let error = validate_install_plan(&plan).unwrap_err();
        assert!(error.to_string().contains("entry limit"));
        assert!(database.read_installed_packages().unwrap().is_empty());

        // The installer independently retains its full archive validation.
        let logger = Logger::new(temp.path());
        logger.init().unwrap();
        let mut installer =
            Installer::new(&database, &logger, PromptHandler::new(true, false, false));
        let error = installer
            .install(plan.last().unwrap().archive.path())
            .unwrap_err();
        assert!(error.to_string().contains("entry limit"));
        assert!(database.read_installed_packages().unwrap().is_empty());
    }

    #[test]
    fn dependency_cycles_are_rejected_before_installation() {
        let temp = TempDir::new().unwrap();
        let database = Database::new(temp.path());
        database.init().unwrap();
        let mut archives = HashMap::new();
        for (name, dependency) in [("first", "second"), ("second", "first")] {
            let source = temp.path().join(format!("source-{name}"));
            fs::create_dir_all(source.join("package/usr/share")).unwrap();
            fs::write(
                source.join("ARKPKG"),
                format!(
                    "Name: {name}\nDescription: cycle test package\nVersion: 1.0\nArch: any\nDependencies:\n{dependency}\n"
                ),
            )
            .unwrap();
            let archive_path = temp.path().join(format!("{name}.ark"));
            create_ark(&source, &archive_path).unwrap();
            archives.insert(name.to_string(), archive_path);
        }

        let error = resolve_install_plan_with(&database, "first", |requirement| {
            let path = archives.get(&requirement.name).unwrap();
            let mut temporary = NamedTempFile::new().unwrap();
            std::io::copy(&mut fs::File::open(path).unwrap(), &mut temporary).unwrap();
            temporary.flush().unwrap();
            Ok(temporary)
        })
        .unwrap_err();
        assert!(error.to_string().contains("first -> second -> first"));
        assert!(database.read_installed_packages().unwrap().is_empty());
    }

    #[test]
    fn installed_providers_satisfy_dependency_requirements() {
        let temp = TempDir::new().unwrap();
        let install_root = temp.path().join("root");
        let database = Database::new(&install_root);
        database.init().unwrap();
        let logger = Logger::new(&install_root);
        logger.init().unwrap();

        let base_source = temp.path().join("source-base");
        fs::create_dir_all(base_source.join("package/usr/share")).unwrap();
        fs::write(
            base_source.join("ARKPKG"),
            "Name: base\nDescription: base package\nVersion: 1.0\nArch: any\nProvides:\nvirtual-base=/usr/share/base\n",
        )
        .unwrap();
        let base_archive = temp.path().join("base.ark");
        create_ark(&base_source, &base_archive).unwrap();
        let mut installer =
            Installer::new(&database, &logger, PromptHandler::new(true, false, false));
        installer.install(&base_archive).unwrap();

        let app_source = temp.path().join("source-app");
        fs::create_dir_all(app_source.join("package/usr/share")).unwrap();
        fs::write(
            app_source.join("ARKPKG"),
            "Name: app\nDescription: depends on a provided feature\nVersion: 1.0\nArch: any\nDependencies:\nvirtual-base>=1.0\n",
        )
        .unwrap();
        let app_archive = temp.path().join("app.ark");
        create_ark(&app_source, &app_archive).unwrap();

        let plan = resolve_install_plan_with(&database, "app", |requirement| {
            assert_eq!(requirement.name, "app");
            let mut temporary = NamedTempFile::new().unwrap();
            std::io::copy(&mut fs::File::open(&app_archive).unwrap(), &mut temporary).unwrap();
            temporary.flush().unwrap();
            Ok(temporary)
        })
        .unwrap();
        let names: Vec<_> = plan
            .iter()
            .map(|package| package.metadata.name.as_str())
            .collect();
        assert_eq!(names, ["app"]);
    }
}
