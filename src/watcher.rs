use std::{
    path::PathBuf,
    process,
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{
    config::{ProjectPaths, TimelineConfig, WATCHER_HEARTBEAT_MS},
    debounce::Debouncer,
    filter::ProjectFilter,
    git::detect_runtime_context,
    snapshot::{capture_initial_state, process_path_change},
    store::TimelineStore,
    time::now_ms,
    ui::{BadgeTone, Ui},
};

pub fn watch(root: PathBuf, config: TimelineConfig) -> Result<()> {
    let paths = ProjectPaths::initialize(&root)?;
    let filter = ProjectFilter::new(paths.root.clone(), config.max_file_size_bytes)?;
    let mut store = TimelineStore::open(&paths, config)?;

    let started_at_ms = now_ms();
    let pid = process::id() as i64;
    store.touch_watcher(pid, started_at_ms, started_at_ms, &paths.root)?;
    let stdout_ui = Ui::stdout();
    let stderr_ui = Ui::stderr();
    let mut runtime_context = detect_runtime_context(&paths.root)?;
    let mut context_id = store.ensure_context(&runtime_context, started_at_ms)?;

    let scan = capture_initial_state(&paths.root, &filter, &mut store, started_at_ms, context_id)?;
    println!(
        "{}",
        stdout_ui.title("Watching", paths.root.display().to_string())
    );
    println!(
        "{}",
        stdout_ui.key_values(&[
            ("Watcher", stdout_ui.badge("RUNNING", BadgeTone::Success)),
            ("Pid", pid.to_string()),
            ("Initial changes", scan.tracked_files.to_string()),
            ("Initial deletions", scan.deleted_files.to_string()),
        ])
    );

    let (sender, receiver) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = sender.send(result);
        },
        NotifyConfig::default(),
    )
    .context("failed to start filesystem watcher")?;
    watcher
        .watch(&paths.root, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", paths.root.display()))?;

    let mut debouncer = Debouncer::new(Duration::from_millis(config.debounce_ms));
    let mut last_heartbeat = Instant::now();

    loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(event)) => {
                let observed_at = Instant::now();
                for path in event.paths {
                    debouncer.record(path, observed_at);
                }
            }
            Ok(Err(error)) => {
                eprintln!("{}", stderr_ui.error(format!("watch error: {error}")));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();
        let loop_now_ms = now_ms();
        let next_context = detect_runtime_context(&paths.root)?;
        if next_context.fingerprint() != runtime_context.fingerprint() {
            runtime_context = next_context;
            context_id = store.ensure_context(&runtime_context, loop_now_ms)?;
            let _ =
                capture_initial_state(&paths.root, &filter, &mut store, loop_now_ms, context_id)?;
        }

        let ready_paths = debouncer.drain_ready(now);
        if !ready_paths.is_empty() {
            context_id = store.ensure_context(&runtime_context, loop_now_ms)?;
            for path in ready_paths {
                process_path_change(
                    &paths.root,
                    &path,
                    &filter,
                    &mut store,
                    loop_now_ms,
                    context_id,
                )?;
            }
        }

        if last_heartbeat.elapsed() >= Duration::from_millis(WATCHER_HEARTBEAT_MS) {
            store.touch_watcher(pid, started_at_ms, loop_now_ms, &paths.root)?;
            last_heartbeat = Instant::now();
        }
    }

    Ok(())
}
