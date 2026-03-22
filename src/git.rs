use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitContext {
    pub worktree_root: String,
    pub common_git_dir: String,
    pub branch_name: Option<String>,
    pub head_commit: Option<String>,
    pub detached_head: bool,
}

impl GitContext {
    pub fn fingerprint(&self) -> String {
        format!(
            "git|worktree={}|common={}|branch={}|head={}|detached={}",
            self.worktree_root,
            self.common_git_dir,
            self.branch_name.as_deref().unwrap_or("-"),
            self.head_commit.as_deref().unwrap_or("-"),
            self.detached_head,
        )
    }

    pub fn short_head(&self) -> Option<String> {
        self.head_commit
            .as_deref()
            .map(|head| head.chars().take(12).collect())
    }

    pub fn display_label(&self) -> String {
        let branch = self
            .branch_name
            .clone()
            .unwrap_or_else(|| "detached".to_string());
        match self.short_head() {
            Some(head) => format!("{branch} @ {head}"),
            None => branch,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextKind {
    Local,
    Git,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContext {
    kind: ContextKind,
    fingerprint: String,
    project_root: String,
    git: Option<GitContext>,
}

impl RuntimeContext {
    pub fn local(root: &Path) -> Result<Self> {
        let project_root = canonicalize_path(root)?;
        Ok(Self {
            kind: ContextKind::Local,
            fingerprint: format!("local|root={project_root}"),
            project_root,
            git: None,
        })
    }

    pub fn from_git(context: GitContext) -> Self {
        Self {
            kind: ContextKind::Git,
            fingerprint: context.fingerprint(),
            project_root: context.worktree_root.clone(),
            git: Some(context),
        }
    }

    pub fn kind(&self) -> ContextKind {
        self.kind
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn project_root(&self) -> &str {
        &self.project_root
    }

    pub fn git(&self) -> Option<&GitContext> {
        self.git.as_ref()
    }

    pub fn display_label(&self) -> String {
        match self.git() {
            Some(context) => context.display_label(),
            None => "local timeline".to_string(),
        }
    }
}

pub fn detect_runtime_context(root: &Path) -> Result<RuntimeContext> {
    match detect_context(root)? {
        Some(context) => Ok(RuntimeContext::from_git(context)),
        None => RuntimeContext::local(root),
    }
}

pub fn detect_context(root: &Path) -> Result<Option<GitContext>> {
    let Some(inside_worktree) = git_output(root, &["rev-parse", "--is-inside-work-tree"])? else {
        return Ok(None);
    };
    if inside_worktree != "true" {
        return Ok(None);
    }

    let worktree_root = git_required_output(root, &["rev-parse", "--show-toplevel"], "worktree")?;
    let worktree_root_path = PathBuf::from(&worktree_root);
    let common_git_dir_raw =
        git_required_output(root, &["rev-parse", "--git-common-dir"], "git common dir")?;
    let common_git_dir = canonicalize_git_dir(&worktree_root_path, &common_git_dir_raw)?;
    let branch_name = git_output(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let head_commit = git_output(root, &["rev-parse", "--verify", "HEAD"])?;
    let detached_head = branch_name.is_none() && head_commit.is_some();

    Ok(Some(GitContext {
        worktree_root: canonicalize_path(&worktree_root_path)?,
        common_git_dir,
        branch_name,
        head_commit,
        detached_head,
    }))
}

fn git_required_output(root: &Path, args: &[&str], label: &str) -> Result<String> {
    git_output(root, args)?.ok_or_else(|| anyhow!("failed to determine Git {label}"))
}

fn git_output(root: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = match Command::new("git").arg("-C").arg(root).args(args).output() {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to run git {}", args.join(" ")))
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("git {} returned invalid UTF-8", args.join(" ")))?;
    let trimmed = stdout.trim().to_string();
    if trimmed.is_empty() {
        return Ok(None);
    }

    Ok(Some(trimmed))
}

fn canonicalize_git_dir(worktree_root: &Path, git_dir: &str) -> Result<String> {
    let raw = PathBuf::from(git_dir);
    let candidate = if raw.is_absolute() {
        raw
    } else {
        worktree_root.join(raw)
    };
    canonicalize_path(&candidate)
}

fn canonicalize_path(path: &Path) -> Result<String> {
    std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize {}", path.display()))
        .map(|value| value.display().to_string())
}
