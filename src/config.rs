use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use walkdir::WalkDir;

pub const TIMELINE_DIR_NAME: &str = ".timeline";
pub const DATABASE_FILE_NAME: &str = "keiros.db";
pub const DEFAULT_RETENTION_DAYS: i64 = 7;
pub const DEFAULT_DEBOUNCE_MS: u64 = 1_200;
pub const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 1_048_576;
pub const FULL_SNAPSHOT_INTERVAL: i64 = 20;
pub const WATCHER_HEARTBEAT_MS: u64 = 2_000;
pub const WATCHER_STALE_AFTER_MS: i64 = 10_000;
pub const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy)]
pub struct TimelineConfig {
    pub retention_days: i64,
    pub debounce_ms: u64,
    pub max_file_size_bytes: u64,
    pub full_snapshot_interval: i64,
}

impl Default for TimelineConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            max_file_size_bytes: DEFAULT_MAX_FILE_SIZE_BYTES,
            full_snapshot_interval: FULL_SNAPSHOT_INTERVAL,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectPaths {
    pub root: PathBuf,
    pub timeline_dir: PathBuf,
    pub database_path: PathBuf,
}

impl ProjectPaths {
    pub fn from_root(root: PathBuf) -> Self {
        let timeline_dir = root.join(TIMELINE_DIR_NAME);
        let database_path = timeline_dir.join(DATABASE_FILE_NAME);
        Self {
            root,
            timeline_dir,
            database_path,
        }
    }

    pub fn initialize(root: &Path) -> Result<Self> {
        let canonical_root = canonicalize_existing_dir(root)?;
        let paths = Self::from_root(canonical_root);
        std::fs::create_dir_all(&paths.timeline_dir)
            .with_context(|| format!("failed to create {}", paths.timeline_dir.display()))?;
        Ok(paths)
    }

    pub fn discover(start: &Path) -> Result<Self> {
        let canonical_start = if start.exists() {
            std::fs::canonicalize(start)
                .with_context(|| format!("failed to canonicalize {}", start.display()))?
        } else {
            start.to_path_buf()
        };

        let mut cursor = if canonical_start.is_file() {
            canonical_start
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| anyhow!("could not determine a parent directory"))?
        } else {
            canonical_start
        };

        loop {
            let candidate = cursor.join(TIMELINE_DIR_NAME);
            if candidate.is_dir() {
                return Ok(Self::from_root(cursor));
            }

            if !cursor.pop() {
                break;
            }
        }

        bail!(
            "could not find a {} directory. Run `keiros watch` from the project root first.",
            TIMELINE_DIR_NAME
        );
    }
}

pub fn canonicalize_existing_dir(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }

    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }

    std::fs::canonicalize(path).with_context(|| format!("failed to canonicalize {}", path.display()))
}

pub fn resolve_input_path(root: &Path, input: &Path) -> Result<PathBuf> {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };

    if let Ok(relative) = absolute.strip_prefix(root) {
        if contains_parent_dir(relative) {
            bail!("path {} escapes the project root", input.display());
        }
        return Ok(absolute);
    }

    bail!(
        "path {} is outside the project root {}",
        input.display(),
        root.display()
    );
}

pub fn normalize_relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = if path.is_absolute() {
        path.strip_prefix(root).with_context(|| {
            format!(
                "path {} is outside the project root {}",
                path.display(),
                root.display()
            )
        })?
    } else {
        path
    };

    if contains_parent_dir(relative) {
        bail!("path {} escapes the project root", path.display());
    }

    let mut pieces = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => pieces.push(value.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => bail!("path {} escapes the project root", path.display()),
        }
    }

    if pieces.is_empty() {
        bail!("path {} does not refer to a file inside the project", path.display());
    }

    Ok(pieces.join("/"))
}

pub fn timeline_storage_size(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let mut bytes = 0_u64;
    for entry in WalkDir::new(path) {
        let entry = entry.with_context(|| format!("failed to walk {}", path.display()))?;
        if entry.file_type().is_file() {
            bytes = bytes.saturating_add(
                entry
                    .metadata()
                    .with_context(|| format!("failed to read {}", entry.path().display()))?
                    .len(),
            );
        }
    }
    Ok(bytes)
}

fn contains_parent_dir(path: &Path) -> bool {
    path.components().any(|component| matches!(component, Component::ParentDir))
}
