use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{
    config::{normalize_relative_path, resolve_input_path},
    store::TimelineStore,
};

#[derive(Debug, Clone)]
pub enum RestoreAction {
    Write { path: PathBuf, content: String },
    Remove { path: PathBuf },
}

impl RestoreAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Write { .. } => "WRITE",
            Self::Remove { .. } => "REMOVE",
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Write { path, .. } | Self::Remove { path } => path,
        }
    }

    pub fn size_bytes(&self) -> Option<usize> {
        match self {
            Self::Write { content, .. } => Some(content.len()),
            Self::Remove { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RestorePlan {
    pub actions: Vec<RestoreAction>,
}

impl RestorePlan {
    pub fn summary(&self) -> ProjectRestoreSummary {
        let mut summary = ProjectRestoreSummary::default();

        for action in &self.actions {
            match action {
                RestoreAction::Write { .. } => summary.restored_files += 1,
                RestoreAction::Remove { .. } => summary.removed_files += 1,
            }
        }

        summary
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProjectRestoreSummary {
    pub restored_files: usize,
    pub removed_files: usize,
}

pub fn plan_restore_file(
    root: &Path,
    store: &TimelineStore,
    file: &Path,
    at_ms: i64,
    context_id: Option<i64>,
) -> Result<RestorePlan> {
    let Some(context_id) = context_id else {
        bail!("no timeline data captured yet for the active context");
    };

    let absolute_path = resolve_input_path(root, file)?;
    let relative_path = normalize_relative_path(root, &absolute_path)?;
    let content = store.file_state_at(&relative_path, at_ms, context_id)?;

    let actions = match content {
        Some(content) => vec![RestoreAction::Write {
            path: absolute_path,
            content,
        }],
        None => vec![RestoreAction::Remove {
            path: absolute_path,
        }],
    };

    Ok(RestorePlan { actions })
}

pub fn plan_restore_project(
    root: &Path,
    store: &TimelineStore,
    at_ms: i64,
    context_id: Option<i64>,
) -> Result<RestorePlan> {
    let Some(context_id) = context_id else {
        return Ok(RestorePlan::default());
    };

    let mut actions = Vec::new();
    for relative_path in store.list_all_paths(context_id)? {
        let absolute_path = root.join(&relative_path);
        match store.file_state_at(&relative_path, at_ms, context_id)? {
            Some(content) => actions.push(RestoreAction::Write {
                path: absolute_path,
                content,
            }),
            None => actions.push(RestoreAction::Remove {
                path: absolute_path,
            }),
        }
    }

    Ok(RestorePlan { actions })
}

pub fn validate_restore_request(allow_cross_context: bool) -> Result<()> {
    if allow_cross_context {
        bail!(
            "--allow-cross-context is not supported in phase 2 because restore is scoped to the active context"
        );
    }

    Ok(())
}

pub fn apply_restore_plan(plan: &RestorePlan) -> Result<ProjectRestoreSummary> {
    let mut summary = ProjectRestoreSummary::default();

    for action in &plan.actions {
        match action {
            RestoreAction::Write { path, content } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent directories for {}", path.display())
                    })?;
                }
                std::fs::write(path, content)
                    .with_context(|| format!("failed to write {}", path.display()))?;
                summary.restored_files += 1;
            }
            RestoreAction::Remove { path } => {
                if path.exists() {
                    std::fs::remove_file(path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                    summary.removed_files += 1;
                }
            }
        }
    }

    Ok(summary)
}
