use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::{
    config::normalize_relative_path,
    filter::ProjectFilter,
    store::TimelineStore,
};

#[derive(Debug, Default, Clone)]
pub struct ScanSummary {
    pub tracked_files: usize,
    pub deleted_files: usize,
}

pub fn capture_initial_state(
    root: &Path,
    filter: &ProjectFilter,
    store: &mut TimelineStore,
    recorded_at_ms: i64,
) -> Result<ScanSummary> {
    let mut summary = ScanSummary::default();
    let mut seen_paths = HashSet::new();

    let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
        if entry.path() == root {
            true
        } else {
            !filter.is_path_ignored(entry.path(), entry.file_type().is_dir())
        }
    });

    for entry in walker {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let Some(text) = filter.read_trackable_text(entry.path())? else {
            continue;
        };

        let relative_path = normalize_relative_path(root, entry.path())?;
        seen_paths.insert(relative_path.clone());
        if store.record_text(&relative_path, &text, recorded_at_ms)? {
            summary.tracked_files += 1;
        }
    }

    for path in store.list_current_live_paths()? {
        if !seen_paths.contains(&path) && store.record_delete(&path, recorded_at_ms)? {
            summary.deleted_files += 1;
        }
    }

    Ok(summary)
}

pub fn process_path_change(
    root: &Path,
    path: &Path,
    filter: &ProjectFilter,
    store: &mut TimelineStore,
    recorded_at_ms: i64,
) -> Result<()> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    if !absolute_path.starts_with(root) {
        return Ok(());
    }

    if filter.is_path_ignored(&absolute_path, absolute_path.is_dir()) {
        return Ok(());
    }

    if absolute_path.exists() {
        let metadata = std::fs::metadata(&absolute_path)
            .with_context(|| format!("failed to read metadata for {}", absolute_path.display()))?;
        if metadata.is_dir() {
            return Ok(());
        }

        if let Some(text) = filter.read_trackable_text(&absolute_path)? {
            let relative_path = normalize_relative_path(root, &absolute_path)?;
            store.record_text(&relative_path, &text, recorded_at_ms)?;
        }
        return Ok(());
    }

    let relative_path = normalize_relative_path(root, &absolute_path)?;
    if store.record_delete(&relative_path, recorded_at_ms)? {
        return Ok(());
    }

    let prefix = format!("{relative_path}/");
    store.record_delete_prefix(&prefix, recorded_at_ms)?;
    Ok(())
}
