pub mod cli;
pub mod config;
pub mod debounce;
pub mod delta;
pub mod filter;
pub mod restore;
pub mod snapshot;
pub mod store;
pub mod time;
pub mod watcher;

use anyhow::{bail, Context, Result};
use clap::Parser;
use similar::TextDiff;

use crate::{
    cli::{Cli, Command},
    config::{normalize_relative_path, resolve_input_path, timeline_storage_size, ProjectPaths, TimelineConfig},
    restore::{restore_file, restore_project},
    store::TimelineStore,
    time::{format_exact_and_human, parse_timestamp},
};

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = TimelineConfig::default();

    match cli.command {
        Command::Watch { path } => {
            let root = path.unwrap_or(std::env::current_dir().context("failed to read cwd")?);
            watcher::watch(root, config)
        }
        Command::History { file } => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let root = paths.root.clone();
            let absolute = resolve_input_path(&root, &file)?;
            let relative = normalize_relative_path(&root, &absolute)?;
            let store = TimelineStore::open(&paths, config)?;
            let history = store.file_history(&relative)?;

            if history.is_empty() {
                println!("No history recorded for {relative}");
                return Ok(());
            }

            println!("History for {relative}");
            for entry in history {
                let size = entry
                    .size_bytes
                    .map(|value| format!("{value} B"))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{} | {:>6} | {:>5} | {}",
                    format_exact_and_human(entry.recorded_at_ms),
                    entry.event_type,
                    size,
                    entry.storage_kind
                );
            }
            Ok(())
        }
        Command::Diff { file, at, at2 } => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let root = paths.root.clone();
            let absolute = resolve_input_path(&root, &file)?;
            let relative = normalize_relative_path(&root, &absolute)?;
            let store = TimelineStore::open(&paths, config)?;
            let first = parse_timestamp(&at)?;
            let second = parse_timestamp(&at2)?;
            let left = store.file_state_at(&relative, first)?;
            let right = store.file_state_at(&relative, second)?;

            if left.is_none() && right.is_none() {
                println!(
                    "{relative} did not exist at either {} or {}",
                    format_exact_and_human(first),
                    format_exact_and_human(second)
                );
                return Ok(());
            }

            let left_label = format!("{relative} @ {}", format_exact_and_human(first));
            let right_label = format!("{relative} @ {}", format_exact_and_human(second));
            let diff = TextDiff::from_lines(left.as_deref().unwrap_or(""), right.as_deref().unwrap_or(""));
            println!(
                "{}",
                diff.unified_diff()
                    .header(&left_label, &right_label)
                    .to_string()
            );
            Ok(())
        }
        Command::RestoreFile { file, at } => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let store = TimelineStore::open(&paths, config)?;
            let target = parse_timestamp(&at)?;
            let outcome = restore_file(&paths.root, &store, &file, target)?;
            println!(
                "{} {} at {}",
                outcome.verb(),
                outcome.path.display(),
                format_exact_and_human(target)
            );
            Ok(())
        }
        Command::RestoreProject { at } => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let store = TimelineStore::open(&paths, config)?;
            let target = parse_timestamp(&at)?;
            let summary = restore_project(&paths.root, &store, target)?;
            println!(
                "Restored project to {}: {} files written, {} files removed",
                format_exact_and_human(target),
                summary.restored_files,
                summary.removed_files
            );
            Ok(())
        }
        Command::Status => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let store = TimelineStore::open(&paths, config)?;
            let status = store.status(time::now_ms())?;
            let storage_bytes = timeline_storage_size(&paths.timeline_dir)?;
            println!("Project root: {}", paths.root.display());
            println!("Watcher active: {}", if status.watcher.active { "yes" } else { "no" });
            if let Some(heartbeat) = status.watcher.heartbeat_ms {
                println!("Last heartbeat: {}", format_exact_and_human(heartbeat));
            }
            if let Some(pid) = status.watcher.pid {
                println!("Watcher pid: {pid}");
            }
            println!("Tracked files: {}", status.tracked_files);
            println!("Live tracked files: {}", status.live_files);
            println!("Stored revisions: {}", status.revision_count);
            println!("Retention: {} days", config.retention_days);
            println!("Debounce window: {} ms", config.debounce_ms);
            println!("Max tracked size: {} bytes", config.max_file_size_bytes);
            println!("Storage usage: {} bytes", storage_bytes);
            Ok(())
        }
        Command::Prune => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let mut store = TimelineStore::open(&paths, config)?;
            let summary = store.prune(time::now_ms())?;
            println!(
                "Pruned history: removed {} revisions, removed {} file entries, kept {} anchors",
                summary.removed_revisions,
                summary.removed_files,
                summary.kept_anchors
            );
            Ok(())
        }
        Command::Recent { limit } => {
            if limit == 0 {
                bail!("--limit must be greater than zero");
            }
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let store = TimelineStore::open(&paths, config)?;
            let changes = store.recent_changes(limit)?;
            if changes.is_empty() {
                println!("No changes captured yet");
                return Ok(());
            }
            for change in changes {
                println!(
                    "{} | {:>6} | {}",
                    format_exact_and_human(change.recorded_at_ms),
                    change.event_type,
                    change.path
                );
            }
            Ok(())
        }
    }
}
