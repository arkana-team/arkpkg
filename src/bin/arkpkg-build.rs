use clap::Parser;
use std::fs;
use std::path::PathBuf;
use std::process;

use arkpkg::archive::create_ark;
use arkpkg::errors::{ArkError, Result};
use arkpkg::metadata::Metadata;

#[derive(Parser, Debug)]
#[command(
    name = "arkpkg-build",
    author = "arkanaOS Team",
    version = "1.0.0",
    about = "Package builder for arkanaOS packages"
)]
struct BuildCli {
    /// Path to package source directory containing ARKPKG and package/
    package_dir: PathBuf,

    /// Optional output destination directory or file path
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,
}

fn run() -> Result<()> {
    let cli = BuildCli::parse();
    let pkg_dir = cli.package_dir;

    if !pkg_dir.exists() || !pkg_dir.is_dir() {
        return Err(ArkError::InvalidPackage(format!(
            "Directory '{}' does not exist or is not a directory",
            pkg_dir.display()
        )));
    }

    let arkpkg_path = pkg_dir.join("ARKPKG");
    if !arkpkg_path.exists() {
        return Err(ArkError::InvalidPackage(format!(
            "Missing 'ARKPKG' metadata file in '{}'",
            pkg_dir.display()
        )));
    }

    let content = fs::read_to_string(&arkpkg_path)?;
    let metadata = Metadata::parse(&content)?;

    let pkg_files_dir = pkg_dir.join("package");
    if !pkg_files_dir.exists() || !pkg_files_dir.is_dir() {
        return Err(ArkError::InvalidPackage(format!(
            "Missing 'package/' directory in '{}'",
            pkg_dir.display()
        )));
    }

    println!("Building package...");
    println!("Compressing...");

    let ark_name = format!(
        "{}-{}-{}.ark",
        metadata.name, metadata.version, metadata.arch
    );
    let output_path = match cli.output {
        Some(out) => {
            if out.is_dir() {
                out.join(ark_name)
            } else {
                out
            }
        }
        None => PathBuf::from(ark_name),
    };

    create_ark(&pkg_dir, &output_path)?;

    println!("Writing {}", output_path.display());
    println!("Done.");

    Ok(())
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Unexpected internal error: {}", info);
    }));

    if let Err(err) = run() {
        eprintln!("Build failed: {}", err);
        process::exit(err.exit_code());
    }
}
