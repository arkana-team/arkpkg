use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "arkpkg",
    author = "arkanaOS Team",
    version = "1.0.0",
    about = "Official package manager for arkanaOS"
)]
pub struct Cli {
    /// Automatically answer 'Yes' to all prompts
    #[arg(short = 'y', long = "yes", global = true)]
    pub auto_yes: bool,

    /// Automatically answer 'No' to all prompts
    #[arg(short = 'n', long = "no", global = true)]
    pub auto_no: bool,

    /// Force operation, skipping safety checks where appropriate
    #[arg(short = 'f', long = "force", global = true)]
    pub force: bool,

    /// Target installation root directory (default: / or $ARKPKG_ROOT)
    #[arg(long = "root", env = "ARKPKG_ROOT", global = true)]
    pub root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Install a package (.ark archive)
    Install {
        /// Path to .ark package archive
        package_file: PathBuf,
    },

    /// Remove an installed package
    Remove {
        /// Package name
        package_name: String,
    },

    /// Verify integrity of an installed package
    Verify {
        /// Package name
        package_name: String,
    },

    /// Display information about an installed package
    Info {
        /// Package name
        package_name: String,
    },

    /// List all installed packages
    List,

    /// Search installed package names and descriptions
    Search {
        /// Search query term
        query: String,
    },
}
