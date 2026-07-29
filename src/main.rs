use clap::Parser;
use std::env;
use std::path::PathBuf;
use std::process;

use arkpkg::cli::{Cli, Commands};
use arkpkg::database::Database;
use arkpkg::errors::Result;
use arkpkg::installer::Installer;
use arkpkg::logger::Logger;
use arkpkg::prompt::PromptHandler;
use arkpkg::remover::Remover;
use arkpkg::verifier::Verifier;

fn run() -> Result<()> {
    let cli = Cli::parse();

    let root_path = cli
        .root
        .or_else(|| env::var("ARKPKG_ROOT").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));

    let db = Database::new(&root_path);
    db.init()?;

    let logger = Logger::new(&root_path);
    logger.init()?;

    let prompt = PromptHandler::new(cli.auto_yes, cli.auto_no, cli.force);

    match cli.command {
        Commands::Install { package_file } => {
            let mut installer = Installer::new(&db, &logger, prompt);
            installer.install(package_file)?;
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
                    println!("{}\n{}", name, ver);
                } else if let Ok(info) = db.read_arkinfo(&name) {
                    if info.name.to_lowercase().contains(&query_lower) {
                        println!("{}\n{}", name, ver);
                    }
                }
            }
        }
    }

    Ok(())
}

fn main() {
    // Set clean panic hook to avoid displaying raw Rust panic messages to users
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Unexpected internal error: {}", info);
    }));

    if let Err(err) = run() {
        eprintln!("{}", err);
        process::exit(err.exit_code());
    }
}
