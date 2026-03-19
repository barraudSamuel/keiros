pub mod cli;
pub mod config;
pub mod debounce;
pub mod delta;
pub mod filter;
pub mod restore;
pub mod snapshot;
pub mod store;
pub mod time;
pub mod ui;
pub mod watcher;

use anyhow::{bail, Context, Result};
use clap::Parser;
use similar::TextDiff;

use crate::{
    cli::{Cli, Command},
    config::{
        normalize_relative_path, resolve_input_path, timeline_storage_size, ProjectPaths,
        TimelineConfig,
    },
    restore::{restore_file, restore_project},
    store::TimelineStore,
    time::{format_exact_and_human, parse_timestamp},
    ui::{human_bytes, BadgeTone, TableCell, Ui},
};

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = TimelineConfig::default();
    let ui = Ui::stdout();

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
                println!(
                    "{}",
                    ui.warning(format!("No history recorded for {relative}"))
                );
                return Ok(());
            }

            let rows = history
                .into_iter()
                .map(|entry| {
                    let size = entry
                        .size_bytes
                        .map(|value| human_bytes(value as u64))
                        .unwrap_or_else(|| "-".to_string());
                    vec![
                        TableCell::plain(format_exact_and_human(entry.recorded_at_ms)),
                        ui.event_cell(entry.event_type),
                        TableCell::right(size),
                        ui.storage_cell(entry.storage_kind),
                    ]
                })
                .collect::<Vec<_>>();

            println!("{}", ui.title("History", &relative));
            println!("{}", ui.table(&["When", "Event", "Size", "Storage"], &rows));
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
                    "{}",
                    ui.warning(format!(
                        "{relative} did not exist at either {} or {}",
                        format_exact_and_human(first),
                        format_exact_and_human(second)
                    ))
                );
                return Ok(());
            }

            let left_label = format!("{relative} @ {}", format_exact_and_human(first));
            let right_label = format!("{relative} @ {}", format_exact_and_human(second));
            if left == right {
                println!("{}", ui.title("Diff", &relative));
                println!(
                    "{}",
                    ui.success(format!(
                        "No content differences between {} and {}",
                        format_exact_and_human(first),
                        format_exact_and_human(second)
                    ))
                );
                return Ok(());
            }

            let diff = TextDiff::from_lines(
                left.as_deref().unwrap_or(""),
                right.as_deref().unwrap_or(""),
            );
            println!("{}", ui.title("Diff", &relative));
            println!(
                "{}",
                ui.render_diff(
                    &diff
                        .unified_diff()
                        .header(&left_label, &right_label)
                        .to_string()
                )
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
                "{}",
                ui.success(format!(
                    "{} {} at {}",
                    outcome.verb(),
                    outcome.path.display(),
                    format_exact_and_human(target)
                ))
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
                "{}",
                ui.title("Restore Project", format_exact_and_human(target))
            );
            println!(
                "{}",
                ui.key_values(&[
                    ("Result", ui.badge("DONE", BadgeTone::Success)),
                    ("Files written", summary.restored_files.to_string()),
                    ("Files removed", summary.removed_files.to_string()),
                ])
            );
            Ok(())
        }
        Command::Status => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let store = TimelineStore::open(&paths, config)?;
            let status = store.status(time::now_ms())?;
            let storage_bytes = timeline_storage_size(&paths.timeline_dir)?;
            let watcher_state = if status.watcher.active {
                ui.badge("ACTIVE", BadgeTone::Success)
            } else if status.watcher.pid.is_some() {
                ui.badge("STALE", BadgeTone::Warning)
            } else {
                ui.badge("IDLE", BadgeTone::Danger)
            };

            println!("{}", ui.title("Status", paths.root.display().to_string()));
            println!(
                "{}",
                ui.key_values(&[
                    ("Watcher", watcher_state),
                    (
                        "Started",
                        status
                            .watcher
                            .started_at_ms
                            .map(format_exact_and_human)
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    (
                        "Last heartbeat",
                        status
                            .watcher
                            .heartbeat_ms
                            .map(format_exact_and_human)
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    (
                        "Watcher pid",
                        status
                            .watcher
                            .pid
                            .map(|pid| pid.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    ("Tracked files", status.tracked_files.to_string()),
                    ("Live files", status.live_files.to_string()),
                    ("Stored revisions", status.revision_count.to_string()),
                    ("Retention", format!("{} days", config.retention_days)),
                    ("Debounce", format!("{} ms", config.debounce_ms)),
                    (
                        "Max file size",
                        human_bytes(config.max_file_size_bytes as u64)
                    ),
                    ("Storage usage", human_bytes(storage_bytes)),
                ])
            );
            Ok(())
        }
        Command::Prune => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let mut store = TimelineStore::open(&paths, config)?;
            let summary = store.prune(time::now_ms())?;
            println!(
                "{}",
                ui.success(format!(
                "Pruned history: removed {} revisions, removed {} file entries, kept {} anchors",
                summary.removed_revisions,
                summary.removed_files,
                summary.kept_anchors
            ))
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
                println!("{}", ui.warning("No changes captured yet"));
                return Ok(());
            }

            let rows = changes
                .into_iter()
                .map(|change| {
                    vec![
                        TableCell::plain(format_exact_and_human(change.recorded_at_ms)),
                        ui.event_cell(change.event_type),
                        TableCell::plain(change.path),
                    ]
                })
                .collect::<Vec<_>>();

            println!("{}", ui.title("Recent Changes", format!("limit {limit}")));
            println!("{}", ui.table(&["When", "Event", "Path"], &rows));
            Ok(())
        }
    }
}
