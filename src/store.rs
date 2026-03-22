use std::{collections::BTreeSet, fmt, path::Path, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use sha2::{Digest, Sha256};

use crate::{
    config::{ProjectPaths, TimelineConfig, DAY_MS, WATCHER_STALE_AFTER_MS},
    delta,
    git::{ContextKind, GitContext, RuntimeContext},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Create,
    Modify,
    Delete,
}

impl EventType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Modify => "modify",
            Self::Delete => "delete",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "create" => Ok(Self::Create),
            "modify" => Ok(Self::Modify),
            "delete" => Ok(Self::Delete),
            _ => bail!("unknown event type `{value}`"),
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    Full,
    Patch,
    None,
}

impl StorageKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Patch => "patch",
            Self::None => "none",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "full" => Ok(Self::Full),
            "patch" => Ok(Self::Patch),
            "none" => Ok(Self::None),
            _ => bail!("unknown storage kind `{value}`"),
        }
    }
}

impl fmt::Display for StorageKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
struct RevisionRow {
    id: i64,
    ordinal: i64,
    recorded_at_ms: i64,
    event_type: EventType,
    storage_kind: StorageKind,
    base_revision_id: Option<i64>,
    content_hash: Option<String>,
    _size_bytes: Option<i64>,
    payload: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct LegacyFileRow {
    id: i64,
    path: String,
}

#[derive(Debug, Clone)]
struct LegacyRevisionRow {
    recorded_at_ms: i64,
    event_type: EventType,
    storage_kind: StorageKind,
    payload: Option<Vec<u8>>,
    git_context: Option<GitContext>,
}

#[derive(Debug, Clone)]
struct ContextRecord {
    kind: ContextKind,
    fingerprint: String,
    worktree_root: Option<String>,
    common_git_dir: Option<String>,
    branch_name: Option<String>,
    head_commit: Option<String>,
    detached_head: bool,
}

impl ContextRecord {
    fn from_runtime(context: &RuntimeContext) -> Self {
        match context.git() {
            Some(git) => Self::from_git(git),
            None => Self::local(context.project_root()),
        }
    }

    fn from_legacy(project_root: &str, git_context: Option<&GitContext>) -> Self {
        match git_context {
            Some(context) => Self::from_git(context),
            None => Self::local(project_root),
        }
    }

    fn from_git(context: &GitContext) -> Self {
        Self {
            kind: ContextKind::Git,
            fingerprint: context.fingerprint(),
            worktree_root: Some(context.worktree_root.clone()),
            common_git_dir: Some(context.common_git_dir.clone()),
            branch_name: context.branch_name.clone(),
            head_commit: context.head_commit.clone(),
            detached_head: context.detached_head,
        }
    }

    fn local(project_root: &str) -> Self {
        Self {
            kind: ContextKind::Local,
            fingerprint: format!("local|root={project_root}"),
            worktree_root: None,
            common_git_dir: None,
            branch_name: None,
            head_commit: None,
            detached_head: false,
        }
    }

    fn kind_str(&self) -> &'static str {
        match self.kind {
            ContextKind::Local => "local",
            ContextKind::Git => "git",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub recorded_at_ms: i64,
    pub event_type: EventType,
    pub size_bytes: Option<i64>,
    pub storage_kind: StorageKind,
}

#[derive(Debug, Clone)]
pub struct RecentChange {
    pub path: String,
    pub recorded_at_ms: i64,
    pub event_type: EventType,
}

#[derive(Debug, Clone, Default)]
pub struct WatcherStatus {
    pub active: bool,
    pub pid: Option<i64>,
    pub started_at_ms: Option<i64>,
    pub heartbeat_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct StatusSummary {
    pub watcher: WatcherStatus,
    pub tracked_files: i64,
    pub live_files: i64,
    pub revision_count: i64,
}

#[derive(Debug, Clone)]
pub struct PruneSummary {
    pub removed_revisions: usize,
    pub removed_files: usize,
    pub kept_anchors: usize,
}

#[derive(Debug)]
pub struct TimelineStore {
    conn: Connection,
    config: TimelineConfig,
    project_root: String,
}

impl TimelineStore {
    pub fn open(paths: &ProjectPaths, config: TimelineConfig) -> Result<Self> {
        let conn = Connection::open(&paths.database_path)
            .with_context(|| format!("failed to open {}", paths.database_path.display()))?;
        conn.busy_timeout(Duration::from_secs(2))
            .context("failed to configure sqlite busy timeout")?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            ",
        )
        .context("failed to initialize sqlite pragmas")?;

        let mut store = Self {
            conn,
            config,
            project_root: paths.root.display().to_string(),
        };
        store.initialize_schema()?;
        store.check_integrity()?;
        Ok(store)
    }

    pub fn ensure_context(&mut self, context: &RuntimeContext, recorded_at_ms: i64) -> Result<i64> {
        let record = ContextRecord::from_runtime(context);
        let tx = self
            .conn
            .transaction()
            .context("failed to begin sqlite transaction")?;
        let context_id = ensure_context_tx(&tx, &record, recorded_at_ms)?;
        tx.commit().context("failed to commit sqlite transaction")?;
        Ok(context_id)
    }

    pub fn resolve_context_id(&self, context: &RuntimeContext) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM contexts WHERE fingerprint = ?1",
                params![context.fingerprint()],
                |row| row.get(0),
            )
            .optional()
            .context("failed to resolve context id")
    }

    pub fn record_text(
        &mut self,
        relative_path: &str,
        content: &str,
        recorded_at_ms: i64,
        context_id: i64,
    ) -> Result<bool> {
        let hash = hash_text(content);
        let tx = self
            .conn
            .transaction()
            .context("failed to begin sqlite transaction")?;
        touch_context_seen_tx(&tx, context_id, recorded_at_ms)?;
        let file_id = upsert_tracked_file_tx(&tx, context_id, relative_path, recorded_at_ms)?;
        let latest = latest_revision_for_file_tx(&tx, file_id)?;

        if let Some(revision) = latest.as_ref() {
            if revision.event_type != EventType::Delete
                && revision.content_hash.as_deref() == Some(hash.as_str())
            {
                tx.commit().context("failed to commit sqlite transaction")?;
                return Ok(false);
            }
        }

        let ordinal = latest
            .as_ref()
            .map(|revision| revision.ordinal + 1)
            .unwrap_or(1);
        let event_type = match latest.as_ref().map(|revision| revision.event_type) {
            None | Some(EventType::Delete) => EventType::Create,
            Some(_) => EventType::Modify,
        };

        let (storage_kind, base_revision_id, payload) = if event_type == EventType::Modify
            && ordinal % self.config.full_snapshot_interval != 0
        {
            let previous_revision = latest
                .as_ref()
                .ok_or_else(|| anyhow!("missing previous revision for patch"))?;
            let previous_content = reconstruct_revision_content_tx(&tx, previous_revision.id)?;
            (
                StorageKind::Patch,
                Some(previous_revision.id),
                Some(delta::create_patch(&previous_content, content)),
            )
        } else {
            (StorageKind::Full, None, Some(content.as_bytes().to_vec()))
        };

        tx.execute(
            "
            INSERT INTO revisions (
                file_id,
                ordinal,
                recorded_at_ms,
                event_type,
                storage_kind,
                base_revision_id,
                content_hash,
                size_bytes,
                payload
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ",
            params![
                file_id,
                ordinal,
                recorded_at_ms,
                event_type.as_str(),
                storage_kind.as_str(),
                base_revision_id,
                hash,
                content.len() as i64,
                payload,
            ],
        )
        .with_context(|| format!("failed to insert revision for {relative_path}"))?;

        tx.commit().context("failed to commit sqlite transaction")?;
        Ok(true)
    }

    pub fn record_delete(
        &mut self,
        relative_path: &str,
        recorded_at_ms: i64,
        context_id: i64,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction()
            .context("failed to begin sqlite transaction")?;
        touch_context_seen_tx(&tx, context_id, recorded_at_ms)?;
        let Some(file_id) = tracked_file_id_by_path_tx(&tx, context_id, relative_path)? else {
            tx.commit().context("failed to commit sqlite transaction")?;
            return Ok(false);
        };

        tx.execute(
            "UPDATE tracked_files SET last_seen_ms = ?2 WHERE id = ?1",
            params![file_id, recorded_at_ms],
        )
        .with_context(|| format!("failed to update file metadata for {relative_path}"))?;

        let Some(latest) = latest_revision_for_file_tx(&tx, file_id)? else {
            tx.commit().context("failed to commit sqlite transaction")?;
            return Ok(false);
        };

        if latest.event_type == EventType::Delete {
            tx.commit().context("failed to commit sqlite transaction")?;
            return Ok(false);
        }

        tx.execute(
            "
            INSERT INTO revisions (
                file_id,
                ordinal,
                recorded_at_ms,
                event_type,
                storage_kind,
                base_revision_id,
                content_hash,
                size_bytes,
                payload
            ) VALUES (?1, ?2, ?3, 'delete', 'none', NULL, NULL, NULL, NULL)
            ",
            params![file_id, latest.ordinal + 1, recorded_at_ms],
        )
        .with_context(|| format!("failed to record deletion for {relative_path}"))?;

        tx.commit().context("failed to commit sqlite transaction")?;
        Ok(true)
    }

    pub fn record_delete_prefix(
        &mut self,
        prefix: &str,
        recorded_at_ms: i64,
        context_id: i64,
    ) -> Result<usize> {
        let paths = self.list_current_live_paths_under(prefix, context_id)?;
        let mut deleted = 0_usize;
        for path in paths {
            if self.record_delete(&path, recorded_at_ms, context_id)? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    pub fn file_history(&self, relative_path: &str, context_id: i64) -> Result<Vec<HistoryEntry>> {
        let Some(file_id) = self.tracked_file_id_by_path(context_id, relative_path)? else {
            return Ok(Vec::new());
        };

        let mut statement = self.conn.prepare(
            "
            SELECT recorded_at_ms, event_type, size_bytes, storage_kind
            FROM revisions
            WHERE file_id = ?1
            ORDER BY recorded_at_ms DESC, id DESC
            ",
        )?;
        let rows = statement.query_map(params![file_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut history = Vec::new();
        for row in rows {
            let (recorded_at_ms, event_type, size_bytes, storage_kind) = row?;
            history.push(HistoryEntry {
                recorded_at_ms,
                event_type: EventType::from_str(&event_type)?,
                size_bytes,
                storage_kind: StorageKind::from_str(&storage_kind)?,
            });
        }
        Ok(history)
    }

    pub fn file_state_at(
        &self,
        relative_path: &str,
        at_ms: i64,
        context_id: i64,
    ) -> Result<Option<String>> {
        let Some(file_id) = self.tracked_file_id_by_path(context_id, relative_path)? else {
            return Ok(None);
        };

        let Some(revision) = latest_revision_at_conn(&self.conn, file_id, at_ms)? else {
            return Ok(None);
        };

        if revision.event_type == EventType::Delete {
            return Ok(None);
        }

        Ok(Some(reconstruct_revision_content_conn(
            &self.conn,
            revision.id,
        )?))
    }

    pub fn list_all_paths(&self, context_id: i64) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM tracked_files WHERE context_id = ?1 ORDER BY path ASC")
            .context("failed to prepare file listing query")?;
        let rows = statement.query_map(params![context_id], |row| row.get::<_, String>(0))?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row?);
        }
        Ok(paths)
    }

    pub fn list_current_live_paths(&self, context_id: i64) -> Result<Vec<String>> {
        self.list_current_live_paths_under("", context_id)
    }

    pub fn recent_changes(&self, limit: usize, context_id: i64) -> Result<Vec<RecentChange>> {
        let mut statement = self.conn.prepare(
            "
            SELECT f.path, r.recorded_at_ms, r.event_type
            FROM revisions r
            INNER JOIN tracked_files f ON f.id = r.file_id
            WHERE f.context_id = ?1
            ORDER BY r.recorded_at_ms DESC, r.id DESC
            LIMIT ?2
            ",
        )?;
        let rows = statement.query_map(params![context_id, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut changes = Vec::new();
        for row in rows {
            let (path, recorded_at_ms, event_type) = row?;
            changes.push(RecentChange {
                path,
                recorded_at_ms,
                event_type: EventType::from_str(&event_type)?,
            });
        }
        Ok(changes)
    }

    pub fn status(&self, now_ms: i64) -> Result<StatusSummary> {
        let tracked_files = self
            .conn
            .query_row("SELECT COUNT(*) FROM tracked_files", [], |row| row.get(0))
            .context("failed to count tracked files")?;
        let revision_count = self
            .conn
            .query_row("SELECT COUNT(*) FROM revisions", [], |row| row.get(0))
            .context("failed to count revisions")?;
        let live_files = self
            .conn
            .query_row(
                "
                SELECT COUNT(*)
                FROM tracked_files f
                JOIN revisions r ON r.id = (
                    SELECT r2.id
                    FROM revisions r2
                    WHERE r2.file_id = f.id
                    ORDER BY r2.recorded_at_ms DESC, r2.id DESC
                    LIMIT 1
                )
                WHERE r.event_type != 'delete'
                ",
                [],
                |row| row.get(0),
            )
            .context("failed to count live files")?;

        let watcher = self
            .conn
            .query_row(
                "SELECT pid, started_at_ms, heartbeat_ms FROM watcher_state WHERE slot = 1",
                [],
                |row| {
                    Ok(WatcherStatus {
                        active: false,
                        pid: Some(row.get(0)?),
                        started_at_ms: Some(row.get(1)?),
                        heartbeat_ms: Some(row.get(2)?),
                    })
                },
            )
            .optional()
            .context("failed to query watcher state")?
            .unwrap_or_default();

        let mut watcher = watcher;
        watcher.active = watcher
            .heartbeat_ms
            .map(|heartbeat| now_ms.saturating_sub(heartbeat) <= WATCHER_STALE_AFTER_MS)
            .unwrap_or(false);

        Ok(StatusSummary {
            watcher,
            tracked_files,
            live_files,
            revision_count,
        })
    }

    pub fn prune(&mut self, now_ms: i64) -> Result<PruneSummary> {
        let cutoff_ms = now_ms - (self.config.retention_days * DAY_MS);
        let file_ids = {
            let mut statement = self
                .conn
                .prepare("SELECT id FROM tracked_files ORDER BY id ASC")
                .context("failed to prepare file id query")?;
            let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };

        let tx = self
            .conn
            .transaction()
            .context("failed to begin sqlite transaction")?;
        let mut removed_revisions = 0_usize;
        let mut removed_files = 0_usize;
        let mut kept_anchors = 0_usize;

        for file_id in file_ids {
            let revisions = load_all_revisions_for_file_tx(&tx, file_id)?;
            if revisions.is_empty() {
                continue;
            }

            let mut keep_ids = BTreeSet::new();
            let boundary = revisions
                .iter()
                .filter(|revision| revision.recorded_at_ms <= cutoff_ms)
                .max_by_key(|revision| (revision.recorded_at_ms, revision.id))
                .cloned();
            let has_newer = revisions
                .iter()
                .any(|revision| revision.recorded_at_ms > cutoff_ms);

            if has_newer {
                for revision in revisions
                    .iter()
                    .filter(|revision| revision.recorded_at_ms > cutoff_ms)
                {
                    keep_ids.insert(revision.id);
                }
            }

            if let Some(anchor) = boundary {
                keep_ids.insert(anchor.id);
                kept_anchors += 1;
                if anchor.event_type != EventType::Delete {
                    convert_revision_to_full_tx(&tx, anchor.id)?;
                }
            } else if !has_newer {
                continue;
            }

            for revision in revisions {
                if !keep_ids.contains(&revision.id) {
                    tx.execute("DELETE FROM revisions WHERE id = ?1", params![revision.id])
                        .with_context(|| format!("failed to delete revision {}", revision.id))?;
                    removed_revisions += 1;
                }
            }

            let remaining: i64 = tx.query_row(
                "SELECT COUNT(*) FROM revisions WHERE file_id = ?1",
                params![file_id],
                |row| row.get(0),
            )?;
            if remaining == 0 {
                tx.execute("DELETE FROM tracked_files WHERE id = ?1", params![file_id])
                    .with_context(|| format!("failed to delete file row {file_id}"))?;
                removed_files += 1;
            }
        }

        tx.commit().context("failed to commit sqlite transaction")?;
        Ok(PruneSummary {
            removed_revisions,
            removed_files,
            kept_anchors,
        })
    }

    pub fn touch_watcher(
        &self,
        pid: i64,
        started_at_ms: i64,
        heartbeat_ms: i64,
        root: &Path,
    ) -> Result<()> {
        self.conn
            .execute(
                "
            INSERT INTO watcher_state (slot, pid, root_path, started_at_ms, heartbeat_ms)
            VALUES (1, ?1, ?2, ?3, ?4)
            ON CONFLICT(slot) DO UPDATE SET
                pid = excluded.pid,
                root_path = excluded.root_path,
                heartbeat_ms = excluded.heartbeat_ms,
                started_at_ms = CASE
                    WHEN watcher_state.pid = excluded.pid THEN watcher_state.started_at_ms
                    ELSE excluded.started_at_ms
                END
            ",
                params![pid, root.display().to_string(), started_at_ms, heartbeat_ms],
            )
            .context("failed to update watcher heartbeat")?;
        Ok(())
    }

    fn initialize_schema(&mut self) -> Result<()> {
        let user_version = self.current_schema_version()?;
        let has_legacy_files = self.table_exists("files")?;
        let has_contexts = self.table_exists("contexts")?;
        let has_tracked_files = self.table_exists("tracked_files")?;
        let has_revisions = self.table_exists("revisions")?;

        if user_version == 0
            && !has_legacy_files
            && !has_contexts
            && !has_tracked_files
            && !has_revisions
        {
            self.create_v2_schema()?;
            return Ok(());
        }

        if user_version == 0 && has_legacy_files {
            self.migrate_v0_to_v1()?;
        }

        match self.current_schema_version()? {
            1 => self.migrate_v1_to_v2()?,
            2 => self.ensure_v2_support_tables()?,
            version => bail!("unsupported timeline schema version {version}"),
        }

        Ok(())
    }

    fn create_v2_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
            CREATE TABLE IF NOT EXISTS contexts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL CHECK (kind IN ('local', 'git')),
                fingerprint TEXT NOT NULL UNIQUE,
                worktree_root TEXT,
                common_git_dir TEXT,
                branch_name TEXT,
                head_commit TEXT,
                detached_head INTEGER NOT NULL DEFAULT 0,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tracked_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                context_id INTEGER NOT NULL REFERENCES contexts(id) ON DELETE CASCADE,
                path TEXT NOT NULL,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL,
                UNIQUE (context_id, path)
            );

            CREATE TABLE IF NOT EXISTS revisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL REFERENCES tracked_files(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                recorded_at_ms INTEGER NOT NULL,
                event_type TEXT NOT NULL CHECK (event_type IN ('create', 'modify', 'delete')),
                storage_kind TEXT NOT NULL CHECK (storage_kind IN ('full', 'patch', 'none')),
                base_revision_id INTEGER REFERENCES revisions(id) ON DELETE SET NULL,
                content_hash TEXT,
                size_bytes INTEGER,
                payload BLOB
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_tracked_files_context_path
                ON tracked_files (context_id, path);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_revisions_file_ordinal
                ON revisions (file_id, ordinal);
            CREATE INDEX IF NOT EXISTS idx_revisions_file_time
                ON revisions (file_id, recorded_at_ms DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_revisions_time
                ON revisions (recorded_at_ms DESC, id DESC);

            CREATE TABLE IF NOT EXISTS watcher_state (
                slot INTEGER PRIMARY KEY CHECK (slot = 1),
                pid INTEGER NOT NULL,
                root_path TEXT NOT NULL,
                started_at_ms INTEGER NOT NULL,
                heartbeat_ms INTEGER NOT NULL
            );

            PRAGMA user_version = 2;
            ",
            )
            .context("failed to initialize sqlite schema")
    }

    fn ensure_v2_support_tables(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
            CREATE TABLE IF NOT EXISTS watcher_state (
                slot INTEGER PRIMARY KEY CHECK (slot = 1),
                pid INTEGER NOT NULL,
                root_path TEXT NOT NULL,
                started_at_ms INTEGER NOT NULL,
                heartbeat_ms INTEGER NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_tracked_files_context_path
                ON tracked_files (context_id, path);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_revisions_file_ordinal
                ON revisions (file_id, ordinal);
            CREATE INDEX IF NOT EXISTS idx_revisions_file_time
                ON revisions (file_id, recorded_at_ms DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_revisions_time
                ON revisions (recorded_at_ms DESC, id DESC);
            ",
            )
            .context("failed to ensure sqlite support tables")
    }

    fn current_schema_version(&self) -> Result<i64> {
        self.conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("failed to read timeline schema version")
    }

    fn migrate_v0_to_v1(&mut self) -> Result<()> {
        let tx = self
            .conn
            .transaction()
            .context("failed to begin timeline schema migration")?;
        let columns = revision_columns(&tx)?;

        if !columns.contains("git_worktree_root") {
            tx.execute_batch("ALTER TABLE revisions ADD COLUMN git_worktree_root TEXT;")
                .context("failed to add revisions.git_worktree_root")?;
        }
        if !columns.contains("git_common_dir") {
            tx.execute_batch("ALTER TABLE revisions ADD COLUMN git_common_dir TEXT;")
                .context("failed to add revisions.git_common_dir")?;
        }
        if !columns.contains("git_branch_name") {
            tx.execute_batch("ALTER TABLE revisions ADD COLUMN git_branch_name TEXT;")
                .context("failed to add revisions.git_branch_name")?;
        }
        if !columns.contains("git_head_commit") {
            tx.execute_batch("ALTER TABLE revisions ADD COLUMN git_head_commit TEXT;")
                .context("failed to add revisions.git_head_commit")?;
        }
        if !columns.contains("git_detached_head") {
            tx.execute_batch(
                "ALTER TABLE revisions ADD COLUMN git_detached_head INTEGER NOT NULL DEFAULT 0;",
            )
            .context("failed to add revisions.git_detached_head")?;
        }

        tx.execute_batch("PRAGMA user_version = 1;")
            .context("failed to set timeline schema version to 1")?;
        tx.commit()
            .context("failed to commit timeline schema migration")
    }

    fn migrate_v1_to_v2(&mut self) -> Result<()> {
        let project_root = self.project_root.clone();
        let tx = self
            .conn
            .transaction()
            .context("failed to begin phase 2 migration")?;

        tx.execute_batch(
            "
            CREATE TABLE contexts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL CHECK (kind IN ('local', 'git')),
                fingerprint TEXT NOT NULL UNIQUE,
                worktree_root TEXT,
                common_git_dir TEXT,
                branch_name TEXT,
                head_commit TEXT,
                detached_head INTEGER NOT NULL DEFAULT 0,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL
            );

            CREATE TABLE tracked_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                context_id INTEGER NOT NULL REFERENCES contexts(id) ON DELETE CASCADE,
                path TEXT NOT NULL,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL,
                UNIQUE (context_id, path)
            );

            CREATE TABLE revisions_v2 (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL REFERENCES tracked_files(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                recorded_at_ms INTEGER NOT NULL,
                event_type TEXT NOT NULL CHECK (event_type IN ('create', 'modify', 'delete')),
                storage_kind TEXT NOT NULL CHECK (storage_kind IN ('full', 'patch', 'none')),
                base_revision_id INTEGER REFERENCES revisions_v2(id) ON DELETE SET NULL,
                content_hash TEXT,
                size_bytes INTEGER,
                payload BLOB
            );
            ",
        )
        .context("failed to create phase 2 tables")?;

        for legacy_file in load_legacy_files_tx(&tx)? {
            let revisions = load_legacy_revisions_for_file_tx(&tx, legacy_file.id)?;
            if revisions.is_empty() {
                continue;
            }

            let mut current_content: Option<String> = None;
            let mut context_ids = Vec::<(String, i64)>::new();
            let mut tracked_file_ids = Vec::<(String, i64)>::new();
            let mut ordinals = Vec::<(String, i64)>::new();

            for revision in revisions {
                let context =
                    ContextRecord::from_legacy(&project_root, revision.git_context.as_ref());
                let context_id = match context_ids
                    .iter()
                    .find(|(fingerprint, _)| fingerprint == &context.fingerprint)
                    .map(|(_, context_id)| *context_id)
                {
                    Some(context_id) => context_id,
                    None => {
                        let context_id = ensure_context_tx(&tx, &context, revision.recorded_at_ms)?;
                        context_ids.push((context.fingerprint.clone(), context_id));
                        context_id
                    }
                };

                let tracked_file_id = match tracked_file_ids
                    .iter()
                    .find(|(fingerprint, _)| fingerprint == &context.fingerprint)
                    .map(|(_, file_id)| *file_id)
                {
                    Some(file_id) => file_id,
                    None => {
                        let file_id = upsert_tracked_file_v2_tx(
                            &tx,
                            context_id,
                            &legacy_file.path,
                            revision.recorded_at_ms,
                        )?;
                        tracked_file_ids.push((context.fingerprint.clone(), file_id));
                        file_id
                    }
                };

                let ordinal = match ordinals
                    .iter_mut()
                    .find(|(fingerprint, _)| fingerprint == &context.fingerprint)
                {
                    Some((_, ordinal)) => {
                        *ordinal += 1;
                        *ordinal
                    }
                    None => {
                        ordinals.push((context.fingerprint.clone(), 1));
                        1
                    }
                };

                if revision.event_type == EventType::Delete {
                    current_content = None;
                    tx.execute(
                        "
                        INSERT INTO revisions_v2 (
                            file_id,
                            ordinal,
                            recorded_at_ms,
                            event_type,
                            storage_kind,
                            base_revision_id,
                            content_hash,
                            size_bytes,
                            payload
                        ) VALUES (?1, ?2, ?3, 'delete', 'none', NULL, NULL, NULL, NULL)
                        ",
                        params![tracked_file_id, ordinal, revision.recorded_at_ms],
                    )
                    .with_context(|| {
                        format!("failed to migrate delete revision for {}", legacy_file.path)
                    })?;
                    continue;
                }

                let next_content = materialize_legacy_content(&current_content, &revision)?;
                current_content = Some(next_content.clone());
                let hash = hash_text(&next_content);
                tx.execute(
                    "
                    INSERT INTO revisions_v2 (
                        file_id,
                        ordinal,
                        recorded_at_ms,
                        event_type,
                        storage_kind,
                        base_revision_id,
                        content_hash,
                        size_bytes,
                        payload
                    ) VALUES (?1, ?2, ?3, ?4, 'full', NULL, ?5, ?6, ?7)
                    ",
                    params![
                        tracked_file_id,
                        ordinal,
                        revision.recorded_at_ms,
                        revision.event_type.as_str(),
                        hash,
                        next_content.len() as i64,
                        next_content.into_bytes(),
                    ],
                )
                .with_context(|| format!("failed to migrate revision for {}", legacy_file.path))?;
            }
        }

        tx.execute_batch(
            "
            DROP INDEX IF EXISTS idx_revisions_file_ordinal;
            DROP INDEX IF EXISTS idx_revisions_file_time;
            DROP INDEX IF EXISTS idx_revisions_time;
            DROP TABLE revisions;
            DROP TABLE files;
            ALTER TABLE revisions_v2 RENAME TO revisions;

            CREATE UNIQUE INDEX idx_tracked_files_context_path
                ON tracked_files (context_id, path);
            CREATE UNIQUE INDEX idx_revisions_file_ordinal
                ON revisions (file_id, ordinal);
            CREATE INDEX idx_revisions_file_time
                ON revisions (file_id, recorded_at_ms DESC, id DESC);
            CREATE INDEX idx_revisions_time
                ON revisions (recorded_at_ms DESC, id DESC);

            CREATE TABLE IF NOT EXISTS watcher_state (
                slot INTEGER PRIMARY KEY CHECK (slot = 1),
                pid INTEGER NOT NULL,
                root_path TEXT NOT NULL,
                started_at_ms INTEGER NOT NULL,
                heartbeat_ms INTEGER NOT NULL
            );

            PRAGMA user_version = 2;
            ",
        )
        .context("failed to finalize phase 2 migration")?;

        tx.commit().context("failed to commit phase 2 migration")
    }

    fn check_integrity(&self) -> Result<()> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
            .context("failed to run sqlite integrity check")?;
        if result != "ok" {
            bail!("timeline metadata appears corrupted: sqlite quick_check returned `{result}`");
        }
        Ok(())
    }

    fn tracked_file_id_by_path(&self, context_id: i64, relative_path: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM tracked_files WHERE context_id = ?1 AND path = ?2",
                params![context_id, relative_path],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to resolve file id for {relative_path}"))
    }

    fn list_current_live_paths_under(&self, prefix: &str, context_id: i64) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "
            SELECT f.path
            FROM tracked_files f
            JOIN revisions r ON r.id = (
                SELECT r2.id
                FROM revisions r2
                WHERE r2.file_id = f.id
                ORDER BY r2.recorded_at_ms DESC, r2.id DESC
                LIMIT 1
            )
            WHERE f.context_id = ?1
              AND r.event_type != 'delete'
              AND (?2 = '' OR f.path LIKE ?3)
            ORDER BY f.path ASC
            ",
        )?;
        let like_pattern = format!("{prefix}%");
        let rows = statement.query_map(params![context_id, prefix, like_pattern], |row| {
            row.get::<_, String>(0)
        })?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row?);
        }
        Ok(paths)
    }

    fn table_exists(&self, name: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                params![name],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .with_context(|| format!("failed to inspect sqlite table {name}"))
    }
}

fn hash_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn ensure_context_tx(
    tx: &Transaction<'_>,
    context: &ContextRecord,
    recorded_at_ms: i64,
) -> Result<i64> {
    tx.execute(
        "
        INSERT INTO contexts (
            kind,
            fingerprint,
            worktree_root,
            common_git_dir,
            branch_name,
            head_commit,
            detached_head,
            first_seen_ms,
            last_seen_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
        ON CONFLICT(fingerprint) DO UPDATE SET
            kind = excluded.kind,
            worktree_root = excluded.worktree_root,
            common_git_dir = excluded.common_git_dir,
            branch_name = excluded.branch_name,
            head_commit = excluded.head_commit,
            detached_head = excluded.detached_head,
            first_seen_ms = MIN(contexts.first_seen_ms, excluded.first_seen_ms),
            last_seen_ms = MAX(contexts.last_seen_ms, excluded.last_seen_ms)
        ",
        params![
            context.kind_str(),
            context.fingerprint,
            context.worktree_root,
            context.common_git_dir,
            context.branch_name,
            context.head_commit,
            i64::from(context.detached_head),
            recorded_at_ms,
        ],
    )
    .context("failed to upsert context row")?;

    tx.query_row(
        "SELECT id FROM contexts WHERE fingerprint = ?1",
        params![context.fingerprint],
        |row| row.get(0),
    )
    .context("failed to resolve context row")
}

fn touch_context_seen_tx(tx: &Transaction<'_>, context_id: i64, recorded_at_ms: i64) -> Result<()> {
    tx.execute(
        "UPDATE contexts SET last_seen_ms = MAX(last_seen_ms, ?2) WHERE id = ?1",
        params![context_id, recorded_at_ms],
    )
    .context("failed to update context last_seen_ms")?;
    Ok(())
}

fn upsert_tracked_file_tx(
    tx: &Transaction<'_>,
    context_id: i64,
    relative_path: &str,
    recorded_at_ms: i64,
) -> Result<i64> {
    tx.execute(
        "
        INSERT INTO tracked_files (context_id, path, first_seen_ms, last_seen_ms)
        VALUES (?1, ?2, ?3, ?3)
        ON CONFLICT(context_id, path) DO UPDATE SET
            first_seen_ms = MIN(tracked_files.first_seen_ms, excluded.first_seen_ms),
            last_seen_ms = MAX(tracked_files.last_seen_ms, excluded.last_seen_ms)
        ",
        params![context_id, relative_path, recorded_at_ms],
    )
    .with_context(|| format!("failed to upsert file row for {relative_path}"))?;

    tracked_file_id_by_path_tx(tx, context_id, relative_path)?
        .ok_or_else(|| anyhow!("missing file row for {relative_path}"))
}

fn upsert_tracked_file_v2_tx(
    tx: &Transaction<'_>,
    context_id: i64,
    relative_path: &str,
    recorded_at_ms: i64,
) -> Result<i64> {
    tx.execute(
        "
        INSERT INTO tracked_files (context_id, path, first_seen_ms, last_seen_ms)
        VALUES (?1, ?2, ?3, ?3)
        ON CONFLICT(context_id, path) DO UPDATE SET
            first_seen_ms = MIN(tracked_files.first_seen_ms, excluded.first_seen_ms),
            last_seen_ms = MAX(tracked_files.last_seen_ms, excluded.last_seen_ms)
        ",
        params![context_id, relative_path, recorded_at_ms],
    )
    .with_context(|| format!("failed to upsert migrated file row for {relative_path}"))?;

    tx.query_row(
        "SELECT id FROM tracked_files WHERE context_id = ?1 AND path = ?2",
        params![context_id, relative_path],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to resolve migrated file row for {relative_path}"))
}

fn tracked_file_id_by_path_tx(
    tx: &Transaction<'_>,
    context_id: i64,
    relative_path: &str,
) -> Result<Option<i64>> {
    tx.query_row(
        "SELECT id FROM tracked_files WHERE context_id = ?1 AND path = ?2",
        params![context_id, relative_path],
        |row| row.get(0),
    )
    .optional()
    .with_context(|| format!("failed to resolve file id for {relative_path}"))
}

fn latest_revision_at_conn(
    conn: &Connection,
    file_id: i64,
    at_ms: i64,
) -> Result<Option<RevisionRow>> {
    conn.query_row(
        "
        SELECT id, ordinal, recorded_at_ms, event_type, storage_kind, base_revision_id, content_hash, size_bytes, payload
        FROM revisions
        WHERE file_id = ?1 AND recorded_at_ms <= ?2
        ORDER BY recorded_at_ms DESC, id DESC
        LIMIT 1
        ",
        params![file_id, at_ms],
        row_to_revision,
    )
    .optional()
    .context("failed to load revision by timestamp")
}

fn latest_revision_for_file_tx(tx: &Transaction<'_>, file_id: i64) -> Result<Option<RevisionRow>> {
    tx.query_row(
        "
        SELECT id, ordinal, recorded_at_ms, event_type, storage_kind, base_revision_id, content_hash, size_bytes, payload
        FROM revisions
        WHERE file_id = ?1
        ORDER BY recorded_at_ms DESC, id DESC
        LIMIT 1
        ",
        params![file_id],
        row_to_revision,
    )
    .optional()
    .context("failed to load latest revision")
}

fn load_revision_by_id_conn(conn: &Connection, revision_id: i64) -> Result<Option<RevisionRow>> {
    conn.query_row(
        "
        SELECT id, ordinal, recorded_at_ms, event_type, storage_kind, base_revision_id, content_hash, size_bytes, payload
        FROM revisions
        WHERE id = ?1
        ",
        params![revision_id],
        row_to_revision,
    )
    .optional()
    .context("failed to load revision")
}

fn load_revision_by_id_tx(tx: &Transaction<'_>, revision_id: i64) -> Result<Option<RevisionRow>> {
    tx.query_row(
        "
        SELECT id, ordinal, recorded_at_ms, event_type, storage_kind, base_revision_id, content_hash, size_bytes, payload
        FROM revisions
        WHERE id = ?1
        ",
        params![revision_id],
        row_to_revision,
    )
    .optional()
    .context("failed to load revision")
}

fn load_all_revisions_for_file_tx(tx: &Transaction<'_>, file_id: i64) -> Result<Vec<RevisionRow>> {
    let mut statement = tx.prepare(
        "
        SELECT id, ordinal, recorded_at_ms, event_type, storage_kind, base_revision_id, content_hash, size_bytes, payload
        FROM revisions
        WHERE file_id = ?1
        ORDER BY recorded_at_ms ASC, id ASC
        ",
    )?;
    let rows = statement.query_map(params![file_id], row_to_revision)?;
    let mut revisions = Vec::new();
    for row in rows {
        revisions.push(row?);
    }
    Ok(revisions)
}

fn reconstruct_revision_content_conn(conn: &Connection, revision_id: i64) -> Result<String> {
    let revision = load_revision_by_id_conn(conn, revision_id)?
        .ok_or_else(|| anyhow!("missing revision {revision_id}"))?;
    if revision.event_type == EventType::Delete {
        bail!("cannot reconstruct deleted revision {revision_id}");
    }

    let mut chain = vec![revision];
    while chain
        .last()
        .map(|revision| revision.storage_kind != StorageKind::Full)
        .unwrap_or(false)
    {
        let base_id = chain
            .last()
            .and_then(|revision| revision.base_revision_id)
            .ok_or_else(|| anyhow!("revision chain for {revision_id} is missing a base"))?;
        let base_revision = load_revision_by_id_conn(conn, base_id)?
            .ok_or_else(|| anyhow!("missing base revision {base_id}"))?;
        chain.push(base_revision);
    }

    materialize_chain(chain)
}

fn reconstruct_revision_content_tx(tx: &Transaction<'_>, revision_id: i64) -> Result<String> {
    let revision = load_revision_by_id_tx(tx, revision_id)?
        .ok_or_else(|| anyhow!("missing revision {revision_id}"))?;
    if revision.event_type == EventType::Delete {
        bail!("cannot reconstruct deleted revision {revision_id}");
    }

    let mut chain = vec![revision];
    while chain
        .last()
        .map(|revision| revision.storage_kind != StorageKind::Full)
        .unwrap_or(false)
    {
        let base_id = chain
            .last()
            .and_then(|revision| revision.base_revision_id)
            .ok_or_else(|| anyhow!("revision chain for {revision_id} is missing a base"))?;
        let base_revision = load_revision_by_id_tx(tx, base_id)?
            .ok_or_else(|| anyhow!("missing base revision {base_id}"))?;
        chain.push(base_revision);
    }

    materialize_chain(chain)
}

fn materialize_chain(mut chain: Vec<RevisionRow>) -> Result<String> {
    chain.reverse();
    let mut current = String::new();
    for revision in chain {
        match revision.storage_kind {
            StorageKind::Full => {
                let payload = revision
                    .payload
                    .ok_or_else(|| anyhow!("revision {} is missing a full payload", revision.id))?;
                current = String::from_utf8(payload)
                    .with_context(|| format!("revision {} contains invalid UTF-8", revision.id))?;
            }
            StorageKind::Patch => {
                let payload = revision.payload.ok_or_else(|| {
                    anyhow!("revision {} is missing a patch payload", revision.id)
                })?;
                current = delta::apply_patch(&current, &payload)?;
            }
            StorageKind::None => bail!("revision {} has no reconstructable content", revision.id),
        }
    }
    Ok(current)
}

fn convert_revision_to_full_tx(tx: &Transaction<'_>, revision_id: i64) -> Result<()> {
    let Some(revision) = load_revision_by_id_tx(tx, revision_id)? else {
        return Ok(());
    };
    if revision.event_type == EventType::Delete || revision.storage_kind == StorageKind::Full {
        return Ok(());
    }

    let content = reconstruct_revision_content_tx(tx, revision_id)?;
    let hash = hash_text(&content);
    tx.execute(
        "
        UPDATE revisions
        SET storage_kind = 'full',
            base_revision_id = NULL,
            content_hash = ?2,
            size_bytes = ?3,
            payload = ?4
        WHERE id = ?1
        ",
        params![
            revision_id,
            hash,
            content.len() as i64,
            content.into_bytes()
        ],
    )
    .with_context(|| format!("failed to rewrite revision {revision_id} as a full snapshot"))?;
    Ok(())
}

fn row_to_revision(row: &Row<'_>) -> rusqlite::Result<RevisionRow> {
    let event_type: String = row.get(3)?;
    let storage_kind: String = row.get(4)?;
    Ok(RevisionRow {
        id: row.get(0)?,
        ordinal: row.get(1)?,
        recorded_at_ms: row.get(2)?,
        event_type: EventType::from_str(&event_type).map_err(|_| rusqlite::Error::InvalidQuery)?,
        storage_kind: StorageKind::from_str(&storage_kind)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        base_revision_id: row.get(5)?,
        content_hash: row.get(6)?,
        _size_bytes: row.get(7)?,
        payload: row.get(8)?,
    })
}

fn revision_columns(tx: &Transaction<'_>) -> Result<BTreeSet<String>> {
    let mut statement = tx
        .prepare("PRAGMA table_info(revisions)")
        .context("failed to inspect revisions schema")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = BTreeSet::new();
    for row in rows {
        columns.insert(row?);
    }
    Ok(columns)
}

fn load_legacy_files_tx(tx: &Transaction<'_>) -> Result<Vec<LegacyFileRow>> {
    let mut statement = tx.prepare("SELECT id, path FROM files ORDER BY path ASC")?;
    let rows = statement.query_map([], |row| {
        Ok(LegacyFileRow {
            id: row.get(0)?,
            path: row.get(1)?,
        })
    })?;
    let mut files = Vec::new();
    for row in rows {
        files.push(row?);
    }
    Ok(files)
}

fn load_legacy_revisions_for_file_tx(
    tx: &Transaction<'_>,
    file_id: i64,
) -> Result<Vec<LegacyRevisionRow>> {
    let mut statement = tx.prepare(
        "
        SELECT
            recorded_at_ms,
            event_type,
            storage_kind,
            payload,
            git_worktree_root,
            git_common_dir,
            git_branch_name,
            git_head_commit,
            git_detached_head
        FROM revisions
        WHERE file_id = ?1
        ORDER BY recorded_at_ms ASC, id ASC
        ",
    )?;
    let rows = statement.query_map(params![file_id], |row| {
        let event_type: String = row.get(1)?;
        let storage_kind: String = row.get(2)?;
        Ok(LegacyRevisionRow {
            recorded_at_ms: row.get(0)?,
            event_type: EventType::from_str(&event_type)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            storage_kind: StorageKind::from_str(&storage_kind)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            payload: row.get(3)?,
            git_context: legacy_row_to_git_context(row)?,
        })
    })?;
    let mut revisions = Vec::new();
    for row in rows {
        revisions.push(row?);
    }
    Ok(revisions)
}

fn legacy_row_to_git_context(row: &Row<'_>) -> rusqlite::Result<Option<GitContext>> {
    let worktree_root = row.get::<_, Option<String>>(4)?;
    let common_git_dir = row.get::<_, Option<String>>(5)?;
    match (worktree_root, common_git_dir) {
        (Some(worktree_root), Some(common_git_dir)) => Ok(Some(GitContext {
            worktree_root,
            common_git_dir,
            branch_name: row.get(6)?,
            head_commit: row.get(7)?,
            detached_head: row.get::<_, i64>(8)? != 0,
        })),
        _ => Ok(None),
    }
}

fn materialize_legacy_content(
    previous_content: &Option<String>,
    revision: &LegacyRevisionRow,
) -> Result<String> {
    match revision.storage_kind {
        StorageKind::Full => {
            let payload = revision
                .payload
                .as_ref()
                .ok_or_else(|| anyhow!("legacy revision is missing a full payload"))?;
            String::from_utf8(payload.clone()).context("legacy revision contains invalid UTF-8")
        }
        StorageKind::Patch => {
            let payload = revision
                .payload
                .as_ref()
                .ok_or_else(|| anyhow!("legacy revision is missing a patch payload"))?;
            let previous = previous_content
                .as_deref()
                .ok_or_else(|| anyhow!("legacy patch is missing a previous revision"))?;
            delta::apply_patch(previous, payload)
        }
        StorageKind::None => bail!("legacy revision has no reconstructable content"),
    }
}
