use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "cargo-storage", about = "Cargo target directory management")]
pub struct Args {
    /// Directory to scan. Defaults to the home directory.
    pub root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Launch the interactive browser
    Tui {
        /// Directory to scan. Defaults to the home directory.
        root: Option<PathBuf>,
    },
    /// List target dirs on the system.
    List {
        /// Directory to scan. Defaults to the home directory.
        root: Option<PathBuf>,
    },
    /// Delete target dirs older than a max age and larger than a min size.
    Clean {
        /// Directory to scan. Defaults to the home directory.
        root: Option<PathBuf>,
        /// Only candidates older than this age match (e.g. 30d, 6mo, 1y).
        /// Defaults to 30d if no filters are given.
        #[arg(long)]
        older_than: Option<String>,
        /// Only candidates larger than this size match (e.g. 100MB, 1G).
        /// Defaults to 100MB if no filters are given.
        #[arg(long)]
        larger_than: Option<String>,
        /// Delete without prompting for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Print shell completions to stdout.
    Completions {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

impl Args {
    pub fn parse_args() -> Self {
        let mut argv: Vec<String> = std::env::args().collect();
        if argv.get(1).is_some_and(|a| a == "storage") {
            argv.remove(1);
        }
        <Self as Parser>::parse_from(argv)
    }

    /// Scan root for the requested subcommand. A root on the subcommand
    /// wins over the legacy top-level positional.
    pub fn root_for(cmd_root: Option<PathBuf>, top_root: Option<PathBuf>) -> PathBuf {
        cmd_root.or(top_root).unwrap_or_else(|| {
            homedir::my_home()
                .ok()
                .flatten()
                .unwrap_or_else(|| PathBuf::from("."))
        })
    }
}
