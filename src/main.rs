use clap::Parser;
use std::env;
use std::path::PathBuf;
use std::process;

use arkpkg::cli::{Cli, Commands, RepositoryCommands};
use arkpkg::database::Database;
use arkpkg::errors::{ArkError, Result};
use arkpkg::installer::Installer;
use arkpkg::logger::Logger;
use arkpkg::prompt::PromptHandler;
use arkpkg::remover::Remover;
use arkpkg::repository::{RepositoryManager, RepositoryPackage};
use arkpkg::resolver::resolve_install_plan;
use arkpkg::verifier::Verifier;

fn run() -> Result<()> {
    let cli = Cli::parse();

    let root_path = cli
        .root
        .or_else(|| env::var("ARKPKG_ROOT").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));

    let db = Database::new(&root_path);
    db.init()?;
    let repositories = RepositoryManager::new(&root_path);

    let logger = Logger::new(&root_path);
    logger.init()?;

    let mut prompt = PromptHandler::new(cli.auto_yes, cli.auto_no, cli.force);

    match cli.command {
        Commands::Install { package, repo } => {
            let local_path = PathBuf::from(&package);
            if local_path.is_file() {
                let mut installer = Installer::new(&db, &logger, prompt);
                installer.install(local_path)?;
            } else {
                let catalog = repositories.catalog(repo.as_deref())?;
                let plan = resolve_install_plan(&db, &catalog, &package)?;
                if plan.len() > 1 {
                    println!("The following packages will be installed:");
                    for resolved in &plan {
                        println!("  {} {}", resolved.metadata.name, resolved.metadata.version);
                    }
                    if !prompt.ask("Install this package and its dependencies?")? {
                        return Err(arkpkg::errors::ArkError::UserCancelled);
                    }
                }
                let mut installer = Installer::new(&db, &logger, prompt);
                for resolved in plan {
                    installer.install(resolved.archive.path())?;
                }
            }
        }
        Commands::Remove { package_name } => {
            let mut remover = Remover::new(&db, &logger, prompt);
            remover.remove(&package_name)?;
        }
        Commands::Verify { package_name } => {
            let verifier = Verifier::new(&db);
            verifier.verify(&package_name)?;
        }
        Commands::Info { package_name } => {
            let info = db.read_arkinfo(&package_name)?;
            println!("{}", info.to_string_pretty());
        }
        Commands::List => {
            let installed = db.read_installed_packages()?;
            for (name, ver) in installed {
                println!("{} {}", name, ver);
            }
        }
        Commands::Search { query } => {
            let installed = db.read_installed_packages()?;
            let query_lower = query.to_lowercase();
            for (name, ver) in installed {
                let name_matches = name.to_lowercase().contains(&query_lower);
                if name_matches {
                    println!("{} {} [installed]", name, ver);
                }
            }
            for (repo_name, package) in repositories.search(&query, None)? {
                println!("{}", format_repository_search_result(&repo_name, &package));
            }
        }
        Commands::Repo { command } => match command {
            RepositoryCommands::Add { name, url } => {
                repositories.add(&name, &url)?;
                println!(
                    "Added repository '{}' ({})",
                    name,
                    url.trim_end_matches('/')
                );
            }
            RepositoryCommands::Remove { name } => {
                repositories.remove(&name)?;
                println!("Removed repository '{}'", name);
            }
            RepositoryCommands::List => {
                for repository in repositories.list()? {
                    println!("{} {}", repository.name, repository.url);
                }
            }
        },
    }

    Ok(())
}

fn format_repository_search_result(repo_name: &str, package: &RepositoryPackage) -> String {
    escape_terminal_text(&format!(
        "{} {} [{}] ({}) - {}",
        package.name, package.version, package.arch, repo_name, package.description
    ))
}

fn escape_terminal_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    // Untrusted text must not execute terminal controls or inject diagnostic lines.
    for character in text.chars() {
        if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn format_error(error: &ArkError) -> String {
    match error {
        // Escape the field before adding this variant's intentional line break.
        ArkError::MissingDependency(dependency) => {
            ArkError::MissingDependency(escape_terminal_text(dependency)).to_string()
        }
        _ => escape_terminal_text(&error.to_string()),
    }
}

fn main() {
    // Set clean panic hook to avoid displaying raw Rust panic messages to users
    std::panic::set_hook(Box::new(|info| {
        eprintln!(
            "Unexpected internal error: {}",
            escape_terminal_text(&info.to_string())
        );
    }));

    if let Err(err) = run() {
        eprintln!("{}", format_error(&err));
        process::exit(err.exit_code());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_escape_controls_in_all_untrusted_fields() {
        let controls: String = ('\u{0}'..='\u{1f}')
            .chain('\u{7f}'..='\u{9f}')
            .chain(['\u{2028}', '\u{2029}'])
            .collect();
        let errors = [
            ArkError::Generic(controls.clone()),
            ArkError::PackageNotFound(controls.clone()),
            ArkError::MissingDependency(controls.clone()),
            ArkError::ArchitectureMismatch {
                package_arch: controls.clone(),
                system_arch: controls.clone(),
            },
            ArkError::VerificationFailed(controls.clone()),
            ArkError::PackageAlreadyInstalled(controls.clone()),
            ArkError::PackageConflict(controls.clone()),
            ArkError::InvalidPackage(controls.clone()),
            ArkError::PermissionDenied(controls.clone()),
            ArkError::Io(std::io::Error::other(controls)),
        ];
        for error in errors {
            let rendered = format_error(&error);
            let line_breaks = usize::from(matches!(error, ArkError::MissingDependency(_)));
            assert_eq!(rendered.matches('\n').count(), line_breaks);
            assert!(!rendered.chars().any(|c| {
                (c.is_control() && c != '\n') || matches!(c, '\u{2028}' | '\u{2029}')
            }));
            assert!(rendered.contains(r"\u{1b}"));
            assert!(rendered.contains(r"\r"));
            assert!(rendered.contains(r"\n"));
            assert!(rendered.contains(r"\u{2028}\u{2029}"));
        }
    }

    #[test]
    fn errors_preserve_printable_text_and_intentional_formatting() {
        let text = r#"Café 日本語 🦀: "quoted" C:\bin; it's useful!"#;
        let error = ArkError::InvalidPackage(text.into());
        assert_eq!(format_error(&error), error.to_string());
        let error = ArkError::MissingDependency(format!("{text}\nforged\x1b[2J"));
        assert_eq!(
            format_error(&error),
            format!(
                "Missing dependency: {text}\\nforged\\u{{1b}}[2J\nPlease install the required package before continuing."
            )
        );
        assert_eq!(
            format_error(&ArkError::UserCancelled),
            "User cancelled operation"
        );
    }

    fn package() -> RepositoryPackage {
        serde_json::from_str(
            r#"{
                "name": "hello",
                "version": "1.2.3",
                "arch": "any",
                "description": "hello package",
                "file": "packages/hello.ark",
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn repository_search_escapes_controls_decoded_from_json() {
        let mut package = package();
        package.description = serde_json::from_str(
            r#""hello\u001b[2J\u001b[H\r\nforged 9.9 [installed]\t\b\u001b]0;title\u0007\u009b2J""#,
        )
        .unwrap();
        assert!(package.description.contains('\u{1b}'));
        assert_eq!(
            format_repository_search_result("main", &package),
            r"hello 1.2.3 [any] (main) - hello\u{1b}[2J\u{1b}[H\r\nforged 9.9 [installed]\t\u{8}\u{1b}]0;title\u{7}\u{9b}2J"
        );
    }

    #[test]
    fn repository_search_escapes_all_c0_and_c1_controls() {
        let mut package = package();
        package.description = ('\u{0}'..='\u{1f}').chain('\u{7f}'..='\u{9f}').collect();
        // Version validation trims whitespace without changing the index string.
        package.version = "\n1.2.3\r".into();
        let result = format_repository_search_result("main", &package);
        assert!(!result.chars().any(char::is_control));
        assert!(result.starts_with(r"hello \n1.2.3\r [any] (main) - "));
    }

    #[test]
    fn repository_search_preserves_printable_text() {
        let mut package = package();
        for description in [
            "",
            r#"A "quoted" path: C:\bin; it's useful!"#,
            "Café 日本語 🦀",
        ] {
            package.description = description.into();
            assert_eq!(
                format_repository_search_result("main", &package),
                format!("hello 1.2.3 [any] (main) - {description}")
            );
        }
    }
}
