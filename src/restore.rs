use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{
    config::{normalize_relative_path, resolve_input_path},
    store::TimelineStore,
};

#[derive(Debug)]
pub struct FileRestoreOutcome {
    pub path: PathBuf,
    restored: bool,
}

impl FileRestoreOutcome {
    pub fn verb(&self) -> &'static str {
        if self.restored {
            "Restored"
        } else {
            "Removed"
        }
    }
}

#[derive(Debug, Default)]
pub struct ProjectRestoreSummary {
    pub restored_files: usize,
    pub removed_files: usize,
}

pub fn restore_file(
    root: &Path,
    store: &TimelineStore,
    file: &Path,
    at_ms: i64,
) -> Result<FileRestoreOutcome> {
    let absolute_path = resolve_input_path(root, file)?;
    let relative_path = normalize_relative_path(root, &absolute_path)?;
    match store.file_state_at(&relative_path, at_ms)? {
        Some(content) => {
            if let Some(parent) = absolute_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create parent directories for {}", absolute_path.display())
                })?;
            }
            std::fs::write(&absolute_path, content)
                .with_context(|| format!("failed to write {}", absolute_path.display()))?;
            Ok(FileRestoreOutcome {
                path: absolute_path,
                restored: true,
            })
        }
        None => {
            if absolute_path.exists() {
                std::fs::remove_file(&absolute_path)
                    .with_context(|| format!("failed to remove {}", absolute_path.display()))?;
            }
            Ok(FileRestoreOutcome {
                path: absolute_path,
                restored: false,
            })
        }
    }
}

pub fn restore_project(root: &Path, store: &TimelineStore, at_ms: i64) -> Result<ProjectRestoreSummary> {
    let mut summary = ProjectRestoreSummary::default();

    for relative_path in store.list_all_paths()? {
        let absolute_path = root.join(&relative_path);
        match store.file_state_at(&relative_path, at_ms)? {
            Some(content) => {
                if let Some(parent) = absolute_path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent directories for {}", absolute_path.display())
                    })?;
                }
                std::fs::write(&absolute_path, content)
                    .with_context(|| format!("failed to write {}", absolute_path.display()))?;
                summary.restored_files += 1;
            }
            None => {
                if absolute_path.exists() {
                    std::fs::remove_file(&absolute_path)
                        .with_context(|| format!("failed to remove {}", absolute_path.display()))?;
                    summary.removed_files += 1;
                }
            }
        }
    }

    Ok(summary)
}
