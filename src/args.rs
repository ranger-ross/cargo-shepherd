use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "cargo-storage", about = "Cargo target directory management")]
pub struct Args {
    /// Directory to scan. Defaults to the home directory.
    pub root: Option<PathBuf>,

    /// Also scan locally accessible Docker/Podman named volumes.
    #[arg(long)]
    pub docker: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Launch the interactive browser
    Tui {
        /// Directory to scan. Defaults to the home directory.
        root: Option<PathBuf>,
        /// Also scan locally accessible Docker/Podman named volumes.
        #[arg(long)]
        docker: bool,
    },
    /// List target dirs on the system.
    List {
        /// Directory to scan. Defaults to the home directory.
        root: Option<PathBuf>,
        /// Also scan locally accessible Docker/Podman named volumes.
        #[arg(long)]
        docker: bool,
    },
    /// Delete target dirs older than a max age and larger than a min size.
    Clean {
        /// Directory to scan. Defaults to the home directory.
        root: Option<PathBuf>,
        /// Also scan locally accessible Docker/Podman named volumes.
        #[arg(long)]
        docker: bool,
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

    /// Whether `--docker` was passed either on the subcommand or top level.
    pub fn docker_for(cmd_docker: bool, top_docker: bool) -> bool {
        cmd_docker || top_docker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn docker_flag_parses_on_subcommands_and_top_level() {
        let top = Args::try_parse_from(["shepherd", "--docker"]).unwrap();
        assert!(top.docker);
        assert!(top.command.is_none());
        let list = Args::try_parse_from(["shepherd", "list", "--docker"]).unwrap();
        assert!(matches!(
            list.command,
            Some(Command::List { docker: true, .. })
        ));
        // A top-level flag also enables subcommand scans.
        let both = Args::try_parse_from(["shepherd", "--docker", "clean"]).unwrap();
        assert!(both.docker);
        assert!(Args::docker_for(false, both.docker));
        assert!(!Args::docker_for(false, false));
    }
}
