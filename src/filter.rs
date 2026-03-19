use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::config::TIMELINE_DIR_NAME;

const BUILTIN_DIR_IGNORES: &[&str] = &[
    TIMELINE_DIR_NAME,
    ".git",
    "node_modules",
    "dist",
    "build",
    ".next",
    "target",
];

const SECRET_EXTENSIONS: &[&str] = &[".pem", ".key", ".crt", ".p12", ".pfx"];
const TEMP_SUFFIXES: &[&str] = &[".swp", ".swo", ".tmp", ".temp", "~"];

#[derive(Debug)]
pub struct ProjectFilter {
    root: PathBuf,
    gitignore: Gitignore,
    max_file_size_bytes: u64,
}

impl ProjectFilter {
    pub fn new(root: PathBuf, max_file_size_bytes: u64) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(&root);
        let gitignore_path = root.join(".gitignore");
        if gitignore_path.is_file() {
            builder.add(gitignore_path);
        }

        let gitignore = builder.build().context("failed to parse .gitignore")?;
        Ok(Self {
            root,
            gitignore,
            max_file_size_bytes,
        })
    }

    pub fn is_path_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        if relative.as_os_str().is_empty() {
            return false;
        }

        if self.matches_builtin(relative, is_dir) {
            return true;
        }

        self.gitignore
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
    }

    pub fn read_trackable_text(&self, path: &Path) -> Result<Option<String>> {
        if self.is_path_ignored(path, false) {
            return Ok(None);
        }

        let metadata = std::fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        if !metadata.is_file() || metadata.len() > self.max_file_size_bytes {
            return Ok(None);
        }

        let bytes =
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        if bytes.len() as u64 > self.max_file_size_bytes {
            return Ok(None);
        }

        if bytes.iter().any(|byte| *byte == 0) {
            return Ok(None);
        }

        Ok(String::from_utf8(bytes).ok())
    }

    fn matches_builtin(&self, relative: &Path, is_dir: bool) -> bool {
        if relative.components().any(|component| {
            let piece = component.as_os_str().to_string_lossy();
            BUILTIN_DIR_IGNORES.contains(&piece.as_ref())
        }) {
            return true;
        }

        if is_dir {
            return false;
        }

        let file_name = relative
            .file_name()
            .map(|value| value.to_string_lossy())
            .unwrap_or_default();

        if file_name == ".env" || file_name.starts_with(".env.") {
            return true;
        }

        if SECRET_EXTENSIONS
            .iter()
            .any(|extension| file_name.ends_with(extension))
        {
            return true;
        }

        if TEMP_SUFFIXES.iter().any(|suffix| file_name.ends_with(suffix)) {
            return true;
        }

        file_name == ".DS_Store"
            || file_name.starts_with(".#")
            || file_name.contains("credential")
            || file_name.contains("secret")
    }
}
