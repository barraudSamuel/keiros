use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "kairos",
    version,
    about = "Local time-based source history for a project, independent of Git"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start watching a project and recording local history into .timeline/
    Watch {
        /// Project root to watch. Defaults to the current directory.
        path: Option<PathBuf>,
    },
    /// Show historical revisions for a file
    History {
        /// File path relative to the project root
        file: PathBuf,
    },
    /// Show a diff between two historical versions of a file
    Diff {
        /// File path relative to the project root
        file: PathBuf,
        /// First timestamp (RFC3339, unix seconds/millis, or values like 2h ago)
        #[arg(long)]
        at: String,
        /// Second timestamp (RFC3339, unix seconds/millis, or values like 30m ago)
        #[arg(long = "at2")]
        at2: String,
    },
    /// Restore a file to the state it had at a given timestamp
    RestoreFile {
        /// File path relative to the project root
        file: PathBuf,
        /// Target timestamp
        #[arg(long)]
        at: String,
        /// Print the planned restore without changing files
        #[arg(long)]
        dry_run: bool,
        /// Reserved for future cross-context restore support (currently errors in phase 2)
        #[arg(long)]
        allow_cross_context: bool,
    },
    /// Restore every tracked source file in the project to a given timestamp
    RestoreProject {
        /// Target timestamp
        #[arg(long)]
        at: String,
        /// Print the planned restore without changing files
        #[arg(long)]
        dry_run: bool,
        /// Reserved for future cross-context restore support (currently errors in phase 2)
        #[arg(long)]
        allow_cross_context: bool,
    },
    /// Show watcher, retention, and storage status
    Status,
    /// Remove expired history according to retention policy
    Prune,
    /// Show the most recent captured project changes
    Recent {
        /// Maximum number of changes to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}
