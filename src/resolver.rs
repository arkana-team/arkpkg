use std::collections::HashSet;

use tempfile::NamedTempFile;

use crate::archive::extract_ark;
use crate::database::Database;
use crate::errors::{ArkError, Result};
use crate::metadata::{Metadata, ProvideItem, validate_package_name};
use crate::repository::RepositoryCatalog;
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
        satisfied_requirements: HashSet::new(),
        downloaded_bytes: 0,
    };
    resolver.resolve(&root_requirement, true)?;
    Ok(resolver
        .completed
        .into_iter()
        .map(|(package, _)| package)
        .collect())
}

struct IndexedPackage {
    name: String,
    version: Version,
    provides: HashSet<String>,
}

impl IndexedPackage {
    fn new(name: String, version: Version, provides: &[ProvideItem]) -> Self {
        Self {
            name,
            version,
            provides: provides
                .iter()
                .map(|provide| provide.name.to_ascii_lowercase())
                .collect(),
        }
    }

    fn satisfies(&self, normalized_name: &str, requirement: &DependencyReq) -> bool {
        (self.name.eq_ignore_ascii_case(normalized_name) || self.provides.contains(normalized_name))
            && requirement.is_satisfied_by(&self.version)
    }
}

struct DependencyResolver<'a, F> {
    database: &'a Database,
    download: F,
    installed: Vec<IndexedPackage>,
    completed: Vec<(ResolvedPackage, IndexedPackage)>,
    visiting: Vec<IndexedPackage>,
    // Only successful matches are stable as the install plan grows.
    satisfied_requirements: HashSet<String>,
    downloaded_bytes: u64,
}

impl<F> DependencyResolver<'_, F>
where
    F: FnMut(&DependencyReq) -> Result<NamedTempFile>,
{
    fn resolve(&mut self, requirement: &DependencyReq, root: bool) -> Result<()> {
        // Display retains the operator and raw version. Normalize only the name:
        // version strings can contain case-sensitive segments.
        let mut requirement_key = requirement.to_string();
        requirement_key[..requirement.name.len()].make_ascii_lowercase();
        let normalized_name = &requirement_key[..requirement.name.len()];
        if !root && self.satisfied_requirements.contains(&requirement_key) {
            return Ok(());
        }

        if (!root
            && self
                .installed
                .iter()
                .any(|package| package.satisfies(normalized_name, requirement)))
            || self
                .completed
                .iter()
                .any(|(_, package)| package.satisfies(normalized_name, requirement))
        {
            self.satisfied_requirements.insert(requirement_key);
            return Ok(());
        }

        if let Some(index) = self
            .visiting
            .iter()
            .position(|package| package.satisfies(normalized_name, requirement))
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

        if let Some((existing, _)) = self.completed.iter().find(|(package, _)| {
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

        let extraction = crate::temp::directory()?;
        let metadata = extract_ark(archive.path(), extraction.path())?;
        let indexed = IndexedPackage::new(
            metadata.name.clone(),
            metadata.version.clone(),
            &metadata.provides,
        );
        if !indexed.satisfies(normalized_name, requirement) {
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

        self.visiting.push(indexed);
        for dependency in &metadata.dependencies {
            self.resolve(dependency, false)?;
        }
        let indexed = self
            .visiting
            .pop()
            .expect("resolving dependencies retains the visiting package");

        self.completed
            .push((ResolvedPackage { metadata, archive }, indexed));
        self.satisfied_requirements.insert(requirement_key);
        Ok(())
    }
}

fn read_installed(database: &Database) -> Result<Vec<IndexedPackage>> {
    Ok(database
        .read_installed_packages()?
        .into_iter()
        .map(|(name, version)| {
            let provides = database
                .read_arkinfo(&name)
                .map(|info| info.provides)
                .unwrap_or_default();
            IndexedPackage::new(name, version, &provides)
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

    fn test_archive(directory: &TempDir, name: &str, sections: &str) -> std::path::PathBuf {
        let source = directory.path().join(format!("source-{name}"));
        fs::create_dir_all(source.join("package")).unwrap();
        fs::write(
            source.join("ARKPKG"),
            format!("Name: {name}\nDescription: resolver test\nVersion: 1\nArch: any\n{sections}"),
        )
        .unwrap();
        let archive = directory.path().join(format!("{name}.ark"));
        create_ark(source, &archive).unwrap();
        archive
    }

    #[test]
    fn dependency_plan_handles_many_providers_and_repeated_requirements() {
        let temp = TempDir::new().unwrap();
        let database = Database::new(temp.path().join("root"));
        database.init().unwrap();
        let provides = (0..8000)
            .map(|index| format!("v{index:04}\n"))
            .collect::<String>();
        let base = test_archive(&temp, "base", &format!("Provides:\n{provides}vlast\n"));

        for dependencies in [
            "vlast>=1\n".repeat(6000),
            (0..6000).map(|index| format!("v{index:04}>=1\n")).collect(),
        ] {
            let app = test_archive(
                &temp,
                "app",
                &format!("Dependencies:\nbase\n{dependencies}"),
            );
            let mut downloads = Vec::new();
            let plan = resolve_install_plan_with(&database, "app", |requirement| {
                downloads.push(requirement.name.clone());
                let path = match requirement.name.as_str() {
                    "app" => &app,
                    "base" => &base,
                    _ => panic!("provider requirement should already be satisfied"),
                };
                let mut temporary = NamedTempFile::new().unwrap();
                std::io::copy(&mut fs::File::open(path).unwrap(), &mut temporary).unwrap();
                Ok(temporary)
            })
            .unwrap();
            assert_eq!(downloads, ["app", "base"]);
            let names: Vec<_> = plan
                .iter()
                .map(|package| package.metadata.name.as_str())
                .collect();
            assert_eq!(names, ["base", "app"]);
            assert!(database.read_installed_packages().unwrap().is_empty());
        }
    }

    #[test]
    fn metadata_satisfaction_checks_names_provides_and_version_constraints() {
        let metadata = Metadata::parse(
            "Name: shell\nDescription: test shell\nVersion: 2.1\nArch: any\nProvides:\nSh=/bin/sh\nsH=/usr/bin/sh\n",
        )
        .unwrap();
        let package = IndexedPackage::new(metadata.name, metadata.version, &metadata.provides);
        assert_eq!(package.provides.len(), 1);
        for (text, expected) in [
            ("SHELL>=2.0", true),
            ("SH>=2.0", true),
            ("sh>=3.0", false),
            ("sh==2.1.0", true),
            ("sh!=2.1", false),
            ("sh<2.1", false),
            ("sh<=2.1", true),
            ("sh>2.0", true),
            ("missing", false),
        ] {
            let requirement = DependencyReq::parse(text).unwrap();
            assert_eq!(
                package.satisfies(&requirement.name.to_ascii_lowercase(), &requirement),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn cached_satisfaction_preserves_version_constraints() {
        for name in ["base", "vlast"] {
            for constraint in ["<1", ">1", "!=1", "==2"] {
                let temp = TempDir::new().unwrap();
                let database = Database::new(temp.path().join("root"));
                database.init().unwrap();
                let base = test_archive(&temp, "base", "Provides:\nvlast\n");
                let app = test_archive(
                    &temp,
                    "app",
                    &format!("Dependencies:\nbase\n{name}>=1\n{name}>=1\n{name}{constraint}\n"),
                );
                let error = resolve_install_plan_with(&database, "app", |requirement| {
                    let path = match requirement.name.as_str() {
                        "app" => &app,
                        "base" => &base,
                        "vlast" => return Err(ArkError::PackageNotFound(requirement.to_string())),
                        _ => panic!("unexpected download"),
                    };
                    let mut temporary = NamedTempFile::new().unwrap();
                    std::io::copy(&mut fs::File::open(path).unwrap(), &mut temporary).unwrap();
                    Ok(temporary)
                })
                .unwrap_err();
                if name == "base" {
                    assert!(matches!(error, ArkError::PackageConflict(_)));
                } else {
                    assert!(matches!(error, ArkError::PackageNotFound(_)));
                }
                assert!(error.to_string().contains(&format!("{name}{constraint}")));
                assert!(database.read_installed_packages().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn indexed_providers_still_detect_dependency_cycles() {
        let temp = TempDir::new().unwrap();
        let database = Database::new(temp.path().join("root"));
        database.init().unwrap();
        let first = test_archive(
            &temp,
            "first",
            "Dependencies:\nsecond\nProvides:\nvirtual-first\n",
        );
        let second = test_archive(&temp, "second", "Dependencies:\nVIRTUAL-FIRST>=1\n");
        let error = resolve_install_plan_with(&database, "first", |requirement| {
            let path = match requirement.name.as_str() {
                "first" => &first,
                "second" => &second,
                _ => panic!("cycle must be detected before another download"),
            };
            let mut temporary = NamedTempFile::new().unwrap();
            std::io::copy(&mut fs::File::open(path).unwrap(), &mut temporary).unwrap();
            Ok(temporary)
        })
        .unwrap_err();
        assert!(error.to_string().contains("first -> second -> first"));
        assert!(database.read_installed_packages().unwrap().is_empty());
    }

    #[test]
    fn cached_satisfaction_keeps_version_case() {
        let temp = TempDir::new().unwrap();
        let database = Database::new(temp.path().join("root"));
        database.init().unwrap();
        database
            .add_installed_package("base", &Version::parse("1A").unwrap())
            .unwrap();
        let app = test_archive(&temp, "app", "Dependencies:\nbase==1A\nBASE==1a\n");
        let error = resolve_install_plan_with(&database, "app", |requirement| {
            if requirement.name != "app" {
                return Err(ArkError::PackageNotFound(requirement.to_string()));
            }
            let mut temporary = NamedTempFile::new().unwrap();
            std::io::copy(&mut fs::File::open(&app).unwrap(), &mut temporary).unwrap();
            Ok(temporary)
        })
        .unwrap_err();
        assert!(matches!(error, ArkError::PackageNotFound(_)));
        assert!(error.to_string().contains("BASE==1a"));
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
            "Name: app\nDescription: depends on a provided feature\nVersion: 1.0\nArch: any\nDependencies:\nvirtual-base>=1.0\nVIRTUAL-BASE>=1.0\nvirtual-base>0\n",
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
