use std::{collections::BTreeSet, fmt, path::Path, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use sha2::{Digest, Sha256};

use crate::{
    config::{ProjectPaths, TimelineConfig, DAY_MS, WATCHER_STALE_AFTER_MS},
    delta,
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

        let store = Self { conn, config };
        store.initialize_schema()?;
        store.check_integrity()?;
        Ok(store)
    }

    pub fn record_text(&mut self, relative_path: &str, content: &str, recorded_at_ms: i64) -> Result<bool> {
        let hash = hash_text(content);
        let tx = self.conn.transaction().context("failed to begin sqlite transaction")?;
        let file_id = upsert_file_tx(&tx, relative_path, recorded_at_ms)?;
        let latest = latest_revision_for_file_tx(&tx, file_id)?;

        if let Some(revision) = latest.as_ref() {
            if revision.event_type != EventType::Delete
                && revision.content_hash.as_deref() == Some(hash.as_str())
            {
                tx.commit().context("failed to commit sqlite transaction")?;
                return Ok(false);
            }
        }

        let ordinal = latest.as_ref().map(|revision| revision.ordinal + 1).unwrap_or(1);
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

    pub fn record_delete(&mut self, relative_path: &str, recorded_at_ms: i64) -> Result<bool> {
        let tx = self.conn.transaction().context("failed to begin sqlite transaction")?;
        let Some(file_id) = file_id_by_path_tx(&tx, relative_path)? else {
            tx.commit().context("failed to commit sqlite transaction")?;
            return Ok(false);
        };

        tx.execute(
            "UPDATE files SET last_seen_ms = ?2 WHERE id = ?1",
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

    pub fn record_delete_prefix(&mut self, prefix: &str, recorded_at_ms: i64) -> Result<usize> {
        let paths = self.list_current_live_paths_under(prefix)?;
        let mut deleted = 0_usize;
        for path in paths {
            if self.record_delete(&path, recorded_at_ms)? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    pub fn file_history(&self, relative_path: &str) -> Result<Vec<HistoryEntry>> {
        let Some(file_id) = self.file_id_by_path(relative_path)? else {
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
            let event_type: String = row.get(1)?;
            let storage_kind: String = row.get(3)?;
            Ok((row.get::<_, i64>(0)?, event_type, row.get::<_, Option<i64>>(2)?, storage_kind))
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

    pub fn file_state_at(&self, relative_path: &str, at_ms: i64) -> Result<Option<String>> {
        let Some(file_id) = self.file_id_by_path(relative_path)? else {
            return Ok(None);
        };

        let Some(revision) = latest_revision_at_conn(&self.conn, file_id, at_ms)? else {
            return Ok(None);
        };

        if revision.event_type == EventType::Delete {
            return Ok(None);
        }

        Ok(Some(reconstruct_revision_content_conn(&self.conn, revision.id)?))
    }

    pub fn list_all_paths(&self) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT path FROM files ORDER BY path ASC")
            .context("failed to prepare file listing query")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row?);
        }
        Ok(paths)
    }

    pub fn list_current_live_paths(&self) -> Result<Vec<String>> {
        self.list_current_live_paths_under("")
    }

    pub fn recent_changes(&self, limit: usize) -> Result<Vec<RecentChange>> {
        let mut statement = self.conn.prepare(
            "
            SELECT f.path, r.recorded_at_ms, r.event_type
            FROM revisions r
            INNER JOIN files f ON f.id = r.file_id
            ORDER BY r.recorded_at_ms DESC, r.id DESC
            LIMIT ?1
            ",
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
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
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
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
                FROM files f
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
                .prepare("SELECT id FROM files ORDER BY id ASC")
                .context("failed to prepare file id query")?;
            let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            ids
        };

        let tx = self.conn.transaction().context("failed to begin sqlite transaction")?;
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
            let has_newer = revisions.iter().any(|revision| revision.recorded_at_ms > cutoff_ms);

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
                tx.execute("DELETE FROM files WHERE id = ?1", params![file_id])
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

    pub fn touch_watcher(&self, pid: i64, started_at_ms: i64, heartbeat_ms: i64, root: &Path) -> Result<()> {
        self.conn.execute(
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

    fn initialize_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS revisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                recorded_at_ms INTEGER NOT NULL,
                event_type TEXT NOT NULL CHECK (event_type IN ('create', 'modify', 'delete')),
                storage_kind TEXT NOT NULL CHECK (storage_kind IN ('full', 'patch', 'none')),
                base_revision_id INTEGER REFERENCES revisions(id) ON DELETE SET NULL,
                content_hash TEXT,
                size_bytes INTEGER,
                payload BLOB
            );

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
            ",
        )
        .context("failed to initialize sqlite schema")?;
        Ok(())
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

    fn file_id_by_path(&self, relative_path: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params![relative_path],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to resolve file id for {relative_path}"))
    }

    fn list_current_live_paths_under(&self, prefix: &str) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "
            SELECT f.path
            FROM files f
            JOIN revisions r ON r.id = (
                SELECT r2.id
                FROM revisions r2
                WHERE r2.file_id = f.id
                ORDER BY r2.recorded_at_ms DESC, r2.id DESC
                LIMIT 1
            )
            WHERE r.event_type != 'delete'
              AND (?1 = '' OR f.path LIKE ?2)
            ORDER BY f.path ASC
            ",
        )?;
        let like_pattern = format!("{prefix}%");
        let rows = statement.query_map(params![prefix, like_pattern], |row| row.get::<_, String>(0))?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row?);
        }
        Ok(paths)
    }
}

fn hash_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn upsert_file_tx(tx: &Transaction<'_>, relative_path: &str, recorded_at_ms: i64) -> Result<i64> {
    tx.execute(
        "
        INSERT INTO files (path, first_seen_ms, last_seen_ms)
        VALUES (?1, ?2, ?2)
        ON CONFLICT(path) DO UPDATE SET last_seen_ms = excluded.last_seen_ms
        ",
        params![relative_path, recorded_at_ms],
    )
    .with_context(|| format!("failed to upsert file row for {relative_path}"))?;

    file_id_by_path_tx(tx, relative_path)?.ok_or_else(|| anyhow!("missing file row for {relative_path}"))
}

fn file_id_by_path_tx(tx: &Transaction<'_>, relative_path: &str) -> Result<Option<i64>> {
    tx.query_row(
        "SELECT id FROM files WHERE path = ?1",
        params![relative_path],
        |row| row.get(0),
    )
    .optional()
    .with_context(|| format!("failed to resolve file id for {relative_path}"))
}

fn latest_revision_at_conn(conn: &Connection, file_id: i64, at_ms: i64) -> Result<Option<RevisionRow>> {
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
                let payload = revision
                    .payload
                    .ok_or_else(|| anyhow!("revision {} is missing a patch payload", revision.id))?;
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
        params![revision_id, hash, content.len() as i64, content.into_bytes()],
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
