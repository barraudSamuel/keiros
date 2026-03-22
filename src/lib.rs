pub mod cli;
pub mod config;
pub mod debounce;
pub mod delta;
pub mod filter;
pub mod git;
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
    git::detect_runtime_context,
    restore::{
        apply_restore_plan, plan_restore_file, plan_restore_project, validate_restore_request,
        RestoreAction, RestorePlan,
    },
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
            let runtime_context = detect_runtime_context(&paths.root)?;
            let history = match store.resolve_context_id(&runtime_context)? {
                Some(context_id) => store.file_history(&relative, context_id)?,
                None => Vec::new(),
            };

            if history.is_empty() {
                println!(
                    "{}",
                    ui.warning(format!(
                        "No history recorded for {relative} in {}",
                        runtime_context.display_label()
                    ))
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

            println!(
                "{}",
                ui.title(
                    "History",
                    format!("{relative} ({})", runtime_context.display_label())
                )
            );
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
            let runtime_context = detect_runtime_context(&paths.root)?;
            let context_id = store.resolve_context_id(&runtime_context)?;
            let first = parse_timestamp(&at)?;
            let second = parse_timestamp(&at2)?;
            let left = match context_id {
                Some(context_id) => store.file_state_at(&relative, first, context_id)?,
                None => None,
            };
            let right = match context_id {
                Some(context_id) => store.file_state_at(&relative, second, context_id)?,
                None => None,
            };

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
            println!(
                "{}",
                ui.title(
                    "Diff",
                    format!("{relative} ({})", runtime_context.display_label())
                )
            );
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
        Command::RestoreFile {
            file,
            at,
            dry_run,
            allow_cross_context,
        } => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let store = TimelineStore::open(&paths, config)?;
            let runtime_context = detect_runtime_context(&paths.root)?;
            let context_id = store.resolve_context_id(&runtime_context)?;
            let target = parse_timestamp(&at)?;
            validate_restore_request(allow_cross_context)?;
            let plan = plan_restore_file(&paths.root, &store, &file, target, context_id)?;

            if dry_run {
                print_restore_preview(&ui, "Restore File Dry Run", &paths.root, target, &plan);
                return Ok(());
            }

            apply_restore_plan(&plan)?;
            let action = plan
                .actions
                .first()
                .ok_or_else(|| anyhow::anyhow!("restore file plan was empty"))?;
            println!(
                "{}",
                ui.success(format!(
                    "{} {} at {}",
                    match action {
                        RestoreAction::Write { .. } => "Restored",
                        RestoreAction::Remove { .. } => "Removed",
                    },
                    action.path().display(),
                    format_exact_and_human(target)
                ))
            );
            Ok(())
        }
        Command::RestoreProject {
            at,
            dry_run,
            allow_cross_context,
        } => {
            let cwd = std::env::current_dir().context("failed to read cwd")?;
            let paths = ProjectPaths::discover(&cwd)?;
            let store = TimelineStore::open(&paths, config)?;
            let runtime_context = detect_runtime_context(&paths.root)?;
            let context_id = store.resolve_context_id(&runtime_context)?;
            let target = parse_timestamp(&at)?;
            validate_restore_request(allow_cross_context)?;
            let plan = plan_restore_project(&paths.root, &store, target, context_id)?;

            if dry_run {
                print_restore_preview(&ui, "Restore Project Dry Run", &paths.root, target, &plan);
                return Ok(());
            }

            let summary = apply_restore_plan(&plan)?;
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
            let runtime_context = detect_runtime_context(&paths.root)?;
            let storage_bytes = timeline_storage_size(&paths.timeline_dir)?;
            let watcher_state = if status.watcher.active {
                ui.badge("ACTIVE", BadgeTone::Success)
            } else if status.watcher.pid.is_some() {
                ui.badge("STALE", BadgeTone::Warning)
            } else {
                ui.badge("IDLE", BadgeTone::Danger)
            };
            let mut rows = vec![
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
                (
                    "Context kind",
                    if runtime_context.git().is_some() {
                        "git".to_string()
                    } else {
                        "local".to_string()
                    },
                ),
                ("Context", runtime_context.display_label()),
            ];
            if let Some(git) = runtime_context.git() {
                rows.push((
                    "Branch",
                    git.branch_name.clone().unwrap_or_else(|| "-".to_string()),
                ));
                rows.push(("HEAD", git.short_head().unwrap_or_else(|| "-".to_string())));
                rows.push((
                    "Detached",
                    if git.detached_head {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ));
            }
            rows.extend([
                ("Retention", format!("{} days", config.retention_days)),
                ("Debounce", format!("{} ms", config.debounce_ms)),
                (
                    "Max file size",
                    human_bytes(config.max_file_size_bytes as u64),
                ),
                ("Storage usage", human_bytes(storage_bytes)),
            ]);

            println!("{}", ui.title("Status", paths.root.display().to_string()));
            println!("{}", ui.key_values(&rows));
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
            let runtime_context = detect_runtime_context(&paths.root)?;
            let changes = match store.resolve_context_id(&runtime_context)? {
                Some(context_id) => store.recent_changes(limit, context_id)?,
                None => Vec::new(),
            };
            if changes.is_empty() {
                println!(
                    "{}",
                    ui.warning(format!(
                        "No changes captured yet in {}",
                        runtime_context.display_label()
                    ))
                );
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

            println!(
                "{}",
                ui.title(
                    "Recent Changes",
                    format!("{} | limit {limit}", runtime_context.display_label())
                )
            );
            println!("{}", ui.table(&["When", "Event", "Path"], &rows));
            Ok(())
        }
    }
}

fn print_restore_preview(
    ui: &Ui,
    title: &str,
    root: &std::path::Path,
    target: i64,
    plan: &RestorePlan,
) {
    let summary = plan.summary();
    println!("{}", ui.title(title, format_exact_and_human(target)));
    println!(
        "{}",
        ui.key_values(&[
            ("Result", ui.badge("DRY RUN", BadgeTone::Info)),
            ("Files written", summary.restored_files.to_string()),
            ("Files removed", summary.removed_files.to_string()),
        ])
    );

    if plan.actions.is_empty() {
        println!(
            "{}",
            ui.muted("No tracked files matched the requested restore.")
        );
        return;
    }

    let rows = plan
        .actions
        .iter()
        .map(|action| {
            let action_cell = match action {
                RestoreAction::Write { .. } => TableCell::plain(action.label()).success(),
                RestoreAction::Remove { .. } => TableCell::plain(action.label()).danger(),
            };
            let size = action
                .size_bytes()
                .map(|value| human_bytes(value as u64))
                .unwrap_or_else(|| "-".to_string());

            vec![
                action_cell,
                TableCell::plain(display_restore_path(root, action.path())),
                TableCell::right(size),
            ]
        })
        .collect::<Vec<_>>();

    println!("{}", ui.table(&["Action", "Path", "Size"], &rows));
}

fn display_restore_path(root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}
