use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tempfile::tempdir;

use kairos::{
    config::{ProjectPaths, TimelineConfig, DAY_MS, DEFAULT_MAX_FILE_SIZE_BYTES},
    debounce::Debouncer,
    delta,
    filter::ProjectFilter,
    git::{detect_runtime_context, RuntimeContext},
    restore::{
        apply_restore_plan, plan_restore_file, plan_restore_project, validate_restore_request,
    },
    store::TimelineStore,
};

#[test]
fn debounce_waits_for_stable_idle_window() {
    let base = Instant::now();
    let path = Path::new("src/lib.rs").to_path_buf();
    let mut debouncer = Debouncer::new(Duration::from_millis(1_000));

    debouncer.record(path.clone(), base);
    assert!(debouncer
        .drain_ready(base + Duration::from_millis(500))
        .is_empty());

    debouncer.record(path.clone(), base + Duration::from_millis(700));
    assert!(debouncer
        .drain_ready(base + Duration::from_millis(1_500))
        .is_empty());

    let ready = debouncer.drain_ready(base + Duration::from_millis(1_701));
    assert_eq!(ready, vec![path]);
}

#[test]
fn ignore_filter_uses_gitignore_and_builtin_rules() -> Result<()> {
    let temp = tempdir()?;
    write_text(&temp.path().join(".gitignore"), "ignored.rs\n")?;
    write_text(&temp.path().join("src/lib.rs"), "fn main() {}\n")?;
    write_text(&temp.path().join("ignored.rs"), "ignored\n")?;
    write_text(&temp.path().join(".env"), "SECRET=1\n")?;
    write_text(
        &temp.path().join("node_modules/pkg/index.js"),
        "module.exports = {}\n",
    )?;

    let filter = ProjectFilter::new(temp.path().to_path_buf(), DEFAULT_MAX_FILE_SIZE_BYTES)?;
    assert!(filter
        .read_trackable_text(&temp.path().join("src/lib.rs"))?
        .is_some());
    assert!(filter
        .read_trackable_text(&temp.path().join("ignored.rs"))?
        .is_none());
    assert!(filter
        .read_trackable_text(&temp.path().join(".env"))?
        .is_none());
    assert!(filter
        .read_trackable_text(&temp.path().join("node_modules/pkg/index.js"))?
        .is_none());
    Ok(())
}

#[test]
fn oversized_files_are_skipped() -> Result<()> {
    let temp = tempdir()?;
    let large_file = temp.path().join("src/large.rs");
    let content = "a".repeat((DEFAULT_MAX_FILE_SIZE_BYTES + 1) as usize);
    write_text(&large_file, &content)?;

    let filter = ProjectFilter::new(temp.path().to_path_buf(), DEFAULT_MAX_FILE_SIZE_BYTES)?;
    assert!(filter.read_trackable_text(&large_file)?.is_none());
    Ok(())
}

#[test]
fn restore_file_recovers_an_older_version() -> Result<()> {
    let temp = tempdir()?;
    let file_path = temp.path().join("src/main.rs");
    write_text(&file_path, "broken\n")?;

    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text(
        "src/main.rs",
        "fn main() { println!(\"v1\"); }\n",
        1_000,
        context_id,
    )?;
    store.record_text(
        "src/main.rs",
        "fn main() { println!(\"v2\"); }\n",
        2_000,
        context_id,
    )?;

    let plan = plan_restore_file(
        temp.path(),
        &store,
        Path::new("src/main.rs"),
        1_500,
        Some(context_id),
    )?;
    validate_restore_request(false)?;
    apply_restore_plan(&plan)?;

    assert_eq!(
        fs::read_to_string(file_path)?,
        "fn main() { println!(\"v1\"); }\n"
    );
    Ok(())
}

#[test]
fn restore_project_recovers_multiple_files_and_removes_newer_ones() -> Result<()> {
    let temp = tempdir()?;
    write_text(&temp.path().join("src/a.rs"), "current a\n")?;
    write_text(&temp.path().join("src/b.rs"), "current b\n")?;
    write_text(&temp.path().join("src/c.rs"), "current c\n")?;

    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/a.rs", "a1\n", 1_000, context_id)?;
    store.record_text("src/b.rs", "b1\n", 1_000, context_id)?;
    store.record_text("src/a.rs", "a2\n", 2_000, context_id)?;
    store.record_delete("src/b.rs", 3_000, context_id)?;
    store.record_text("src/c.rs", "c1\n", 3_000, context_id)?;

    let plan = plan_restore_project(temp.path(), &store, 1_500, Some(context_id))?;
    validate_restore_request(false)?;
    let summary = apply_restore_plan(&plan)?;

    assert_eq!(summary.restored_files, 2);
    assert_eq!(summary.removed_files, 1);
    assert_eq!(fs::read_to_string(temp.path().join("src/a.rs"))?, "a1\n");
    assert_eq!(fs::read_to_string(temp.path().join("src/b.rs"))?, "b1\n");
    assert!(!temp.path().join("src/c.rs").exists());
    Ok(())
}

#[test]
fn restore_file_dry_run_does_not_modify_disk() -> Result<()> {
    let temp = tempdir()?;
    let file_path = temp.path().join("src/main.rs");
    write_text(&file_path, "current\n")?;

    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/main.rs", "v1\n", 1_000, context_id)?;
    store.record_text("src/main.rs", "v2\n", 2_000, context_id)?;

    let plan = plan_restore_file(
        temp.path(),
        &store,
        Path::new("src/main.rs"),
        1_500,
        Some(context_id),
    )?;
    validate_restore_request(false)?;

    let summary = plan.summary();
    assert_eq!(summary.restored_files, 1);
    assert_eq!(summary.removed_files, 0);
    assert_eq!(fs::read_to_string(file_path)?, "current\n");
    Ok(())
}

#[test]
fn restore_file_errors_when_active_context_has_not_been_captured() -> Result<()> {
    if !git_available() {
        return Ok(());
    }

    let temp = tempdir()?;
    init_git_repo(temp.path())?;
    write_text(&temp.path().join("src/main.rs"), "main branch bytes\n")?;

    let (_paths, mut store) = open_store(temp.path())?;
    let main_context_id = current_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/main.rs", "main branch bytes\n", 1_000, main_context_id)?;

    checkout_new_branch(temp.path(), "feature")?;
    write_text(&temp.path().join("src/main.rs"), "feature branch bytes\n")?;

    let feature_context = detect_runtime_context(temp.path())?;
    let feature_context_id = store.resolve_context_id(&feature_context)?;
    assert!(feature_context_id.is_none());

    let error = plan_restore_file(
        temp.path(),
        &store,
        Path::new("src/main.rs"),
        1_500,
        feature_context_id,
    )
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("no timeline data captured yet for the active context"));
    assert_eq!(
        fs::read_to_string(temp.path().join("src/main.rs"))?,
        "feature branch bytes\n"
    );
    Ok(())
}

#[test]
fn restore_project_dry_run_does_not_modify_disk() -> Result<()> {
    let temp = tempdir()?;
    write_text(&temp.path().join("src/a.rs"), "current a\n")?;
    write_text(&temp.path().join("src/b.rs"), "current b\n")?;
    write_text(&temp.path().join("src/c.rs"), "current c\n")?;

    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/a.rs", "a1\n", 1_000, context_id)?;
    store.record_text("src/b.rs", "b1\n", 1_000, context_id)?;
    store.record_delete("src/b.rs", 3_000, context_id)?;
    store.record_text("src/c.rs", "c1\n", 3_000, context_id)?;

    let plan = plan_restore_project(temp.path(), &store, 1_500, Some(context_id))?;
    validate_restore_request(false)?;

    let summary = plan.summary();
    assert_eq!(summary.restored_files, 2);
    assert_eq!(summary.removed_files, 1);
    assert_eq!(
        fs::read_to_string(temp.path().join("src/a.rs"))?,
        "current a\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src/b.rs"))?,
        "current b\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src/c.rs"))?,
        "current c\n"
    );
    Ok(())
}

#[test]
fn non_git_project_restore_still_works() -> Result<()> {
    let temp = tempdir()?;
    write_text(&temp.path().join("src/lib.rs"), "current\n")?;

    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/lib.rs", "v1\n", 1_000, context_id)?;
    store.record_text("src/lib.rs", "v2\n", 2_000, context_id)?;

    let plan = plan_restore_project(temp.path(), &store, 1_500, Some(context_id))?;
    validate_restore_request(false)?;
    let summary = apply_restore_plan(&plan)?;

    assert_eq!(summary.restored_files, 1);
    assert_eq!(summary.removed_files, 0);
    assert_eq!(fs::read_to_string(temp.path().join("src/lib.rs"))?, "v1\n");
    Ok(())
}

#[test]
fn delete_revisions_make_the_file_absent_after_the_delete_timestamp() -> Result<()> {
    let temp = tempdir()?;
    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/lib.rs", "v1\n", 1_000, context_id)?;
    store.record_delete("src/lib.rs", 2_000, context_id)?;

    assert_eq!(
        store.file_state_at("src/lib.rs", 1_500, context_id)?,
        Some("v1\n".to_string())
    );
    assert_eq!(store.file_state_at("src/lib.rs", 2_500, context_id)?, None);
    Ok(())
}

#[test]
fn pruning_keeps_a_boundary_anchor_and_newer_revisions() -> Result<()> {
    let temp = tempdir()?;
    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 0)?;
    store.record_text("src/lib.rs", "v0\n", 0, context_id)?;
    store.record_text("src/lib.rs", "v1\n", DAY_MS, context_id)?;
    store.record_text("src/lib.rs", "v2\n", 8 * DAY_MS, context_id)?;

    let summary = store.prune(10 * DAY_MS)?;
    assert_eq!(summary.removed_revisions, 1);
    assert_eq!(store.file_history("src/lib.rs", context_id)?.len(), 2);
    assert_eq!(
        store.file_state_at("src/lib.rs", 4 * DAY_MS, context_id)?,
        Some("v1\n".to_string())
    );
    assert_eq!(
        store.file_state_at("src/lib.rs", 8 * DAY_MS, context_id)?,
        Some("v2\n".to_string())
    );
    Ok(())
}

#[test]
fn timestamp_lookup_uses_latest_revision_at_or_before_the_target() -> Result<()> {
    let temp = tempdir()?;
    let (_paths, mut store) = open_store(temp.path())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/lib.rs", "v1\n", 1_000, context_id)?;
    store.record_text("src/lib.rs", "v2\n", 2_000, context_id)?;
    store.record_delete("src/lib.rs", 3_000, context_id)?;
    store.record_text("src/lib.rs", "v3\n", 4_000, context_id)?;

    assert_eq!(
        store.file_state_at("src/lib.rs", 1_999, context_id)?,
        Some("v1\n".to_string())
    );
    assert_eq!(
        store.file_state_at("src/lib.rs", 2_500, context_id)?,
        Some("v2\n".to_string())
    );
    assert_eq!(store.file_state_at("src/lib.rs", 3_500, context_id)?, None);
    assert_eq!(
        store.file_state_at("src/lib.rs", 4_500, context_id)?,
        Some("v3\n".to_string())
    );
    Ok(())
}

#[test]
fn branch_switch_creates_a_new_context_stream() -> Result<()> {
    if !git_available() {
        return Ok(());
    }

    let temp = tempdir()?;
    init_git_repo(temp.path())?;
    let (_paths, mut store) = open_store(temp.path())?;

    let main_context_id = current_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/lib.rs", "main\n", 1_000, main_context_id)?;

    checkout_new_branch(temp.path(), "feature")?;
    let feature_context_id = current_context_id(&mut store, temp.path(), 2_000)?;
    store.record_text("src/lib.rs", "feature\n", 2_000, feature_context_id)?;

    assert_ne!(main_context_id, feature_context_id);
    assert_eq!(
        store.file_state_at("src/lib.rs", 2_500, main_context_id)?,
        Some("main\n".to_string())
    );
    assert_eq!(
        store.file_state_at("src/lib.rs", 2_500, feature_context_id)?,
        Some("feature\n".to_string())
    );
    Ok(())
}

#[test]
fn history_only_shows_current_context_entries() -> Result<()> {
    if !git_available() {
        return Ok(());
    }

    let temp = tempdir()?;
    init_git_repo(temp.path())?;
    let (_paths, mut store) = open_store(temp.path())?;

    let main_context_id = current_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/lib.rs", "main-1\n", 1_000, main_context_id)?;
    store.record_text("src/lib.rs", "main-2\n", 1_500, main_context_id)?;

    checkout_new_branch(temp.path(), "feature")?;
    let feature_context = detect_runtime_context(temp.path())?;
    let feature_context_id = store.ensure_context(&feature_context, 2_000)?;
    store.record_text("src/lib.rs", "feature-1\n", 2_000, feature_context_id)?;

    let history = store.file_history("src/lib.rs", feature_context_id)?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].recorded_at_ms, 2_000);
    assert_eq!(history[0].event_type.to_string(), "create");
    Ok(())
}

#[test]
fn recent_only_shows_current_context_entries() -> Result<()> {
    if !git_available() {
        return Ok(());
    }

    let temp = tempdir()?;
    init_git_repo(temp.path())?;
    let (_paths, mut store) = open_store(temp.path())?;

    let main_context_id = current_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/main.rs", "main\n", 1_000, main_context_id)?;

    checkout_new_branch(temp.path(), "feature")?;
    let feature_context_id = current_context_id(&mut store, temp.path(), 2_000)?;
    store.record_text("src/feature.rs", "feature\n", 2_000, feature_context_id)?;

    let changes = store.recent_changes(10, feature_context_id)?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "src/feature.rs");
    Ok(())
}

#[test]
fn restore_project_only_rebuilds_files_from_current_context() -> Result<()> {
    if !git_available() {
        return Ok(());
    }

    let temp = tempdir()?;
    init_git_repo(temp.path())?;
    let (_paths, mut store) = open_store(temp.path())?;

    let main_context_id = current_context_id(&mut store, temp.path(), 1_000)?;
    store.record_text("src/main_only.rs", "main\n", 1_000, main_context_id)?;

    checkout_new_branch(temp.path(), "feature")?;
    let feature_context_id = current_context_id(&mut store, temp.path(), 2_000)?;
    store.record_text(
        "src/feature_only.rs",
        "feature\n",
        2_000,
        feature_context_id,
    )?;
    store.record_text(
        "src/shared.rs",
        "shared feature\n",
        2_000,
        feature_context_id,
    )?;

    write_text(&temp.path().join("src/main_only.rs"), "disk main\n")?;
    write_text(&temp.path().join("src/feature_only.rs"), "disk feature\n")?;
    write_text(&temp.path().join("src/shared.rs"), "disk shared\n")?;

    let plan = plan_restore_project(temp.path(), &store, 2_500, Some(feature_context_id))?;
    let planned_paths = plan
        .actions
        .iter()
        .map(|action| {
            action
                .path()
                .strip_prefix(temp.path())
                .unwrap()
                .display()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(planned_paths, vec!["src/feature_only.rs", "src/shared.rs"]);

    apply_restore_plan(&plan)?;
    assert_eq!(
        fs::read_to_string(temp.path().join("src/main_only.rs"))?,
        "disk main\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src/feature_only.rs"))?,
        "feature\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src/shared.rs"))?,
        "shared feature\n"
    );
    Ok(())
}

#[test]
fn v1_database_migrates_to_v2() -> Result<()> {
    let temp = tempdir()?;
    let paths = ProjectPaths::initialize(temp.path())?;
    let conn = Connection::open(&paths.database_path)?;
    create_v1_schema(&conn)?;
    conn.execute(
        "INSERT INTO files (id, path, first_seen_ms, last_seen_ms) VALUES (1, 'src/lib.rs', 1000, 1000)",
        [],
    )?;
    conn.execute(
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
            payload,
            git_worktree_root,
            git_common_dir,
            git_branch_name,
            git_head_commit,
            git_detached_head
        ) VALUES (?1, ?2, ?3, 'create', 'full', NULL, ?4, ?5, ?6, NULL, NULL, NULL, NULL, 0)
        ",
        params![1_i64, 1_i64, 1_000_i64, "hash", 3_i64, b"v1\n".to_vec()],
    )?;
    drop(conn);

    let mut store = TimelineStore::open(&paths, TimelineConfig::default())?;
    let context_id = local_context_id(&mut store, temp.path(), 1_500)?;
    assert_eq!(
        store.file_state_at("src/lib.rs", 1_500, context_id)?,
        Some("v1\n".to_string())
    );

    let conn = Connection::open(&paths.database_path)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    assert_eq!(version, 2);
    assert!(!table_exists(&conn, "files")?);
    assert!(table_exists(&conn, "contexts")?);
    assert!(table_exists(&conn, "tracked_files")?);
    assert_eq!(
        table_columns(&conn, "revisions")?,
        vec![
            "id",
            "file_id",
            "ordinal",
            "recorded_at_ms",
            "event_type",
            "storage_kind",
            "base_revision_id",
            "content_hash",
            "size_bytes",
            "payload",
        ]
    );
    Ok(())
}

#[test]
fn phase2_migration_rewrites_patch_chains_as_full_snapshots() -> Result<()> {
    let temp = tempdir()?;
    let paths = ProjectPaths::initialize(temp.path())?;
    let conn = Connection::open(&paths.database_path)?;
    create_v1_schema(&conn)?;
    conn.execute(
        "INSERT INTO files (id, path, first_seen_ms, last_seen_ms) VALUES (1, 'src/lib.rs', 1000, 2000)",
        [],
    )?;
    conn.execute(
        "
        INSERT INTO revisions (
            id,
            file_id,
            ordinal,
            recorded_at_ms,
            event_type,
            storage_kind,
            base_revision_id,
            content_hash,
            size_bytes,
            payload,
            git_worktree_root,
            git_common_dir,
            git_branch_name,
            git_head_commit,
            git_detached_head
        ) VALUES (?1, ?2, ?3, ?4, 'create', 'full', NULL, ?5, ?6, ?7, NULL, NULL, NULL, NULL, 0)
        ",
        params![
            1_i64,
            1_i64,
            1_i64,
            1_000_i64,
            "hash-1",
            3_i64,
            b"v1\n".to_vec()
        ],
    )?;
    let patch = delta::create_patch("v1\n", "v2\n");
    conn.execute(
        "
        INSERT INTO revisions (
            id,
            file_id,
            ordinal,
            recorded_at_ms,
            event_type,
            storage_kind,
            base_revision_id,
            content_hash,
            size_bytes,
            payload,
            git_worktree_root,
            git_common_dir,
            git_branch_name,
            git_head_commit,
            git_detached_head
        ) VALUES (?1, ?2, ?3, ?4, 'modify', 'patch', ?5, ?6, ?7, ?8, NULL, NULL, NULL, NULL, 0)
        ",
        params![2_i64, 1_i64, 2_i64, 2_000_i64, 1_i64, "hash-2", 3_i64, patch],
    )?;
    drop(conn);

    let mut store = TimelineStore::open(&paths, TimelineConfig::default())?;
    let context_id = local_context_id(&mut store, temp.path(), 2_500)?;
    assert_eq!(
        store.file_state_at("src/lib.rs", 2_500, context_id)?,
        Some("v2\n".to_string())
    );

    let conn = Connection::open(&paths.database_path)?;
    let mut statement =
        conn.prepare("SELECT storage_kind, base_revision_id FROM revisions ORDER BY ordinal ASC")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let migrated = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        migrated,
        vec![("full".to_string(), None), ("full".to_string(), None),]
    );
    Ok(())
}

#[test]
fn worktree_a_and_worktree_b_do_not_share_file_streams() -> Result<()> {
    if !git_available() {
        return Ok(());
    }

    let temp = tempdir()?;
    let main_root = temp.path().join("main");
    fs::create_dir_all(&main_root)?;
    init_git_repo(&main_root)?;
    run_git(&main_root, &["branch", "feature"])?;
    let worktree_root = temp.path().join("feature-worktree");
    run_git(
        &main_root,
        &[
            "worktree",
            "add",
            worktree_root.to_str().context("invalid worktree path")?,
            "feature",
        ],
    )?;

    let (_main_paths, mut main_store) = open_store(&main_root)?;
    let (_worktree_paths, mut worktree_store) = open_store(&worktree_root)?;

    let main_context_id = current_context_id(&mut main_store, &main_root, 1_000)?;
    let worktree_context_id = current_context_id(&mut worktree_store, &worktree_root, 2_000)?;
    main_store.record_text("src/lib.rs", "main worktree\n", 1_000, main_context_id)?;
    worktree_store.record_text(
        "src/lib.rs",
        "feature worktree\n",
        2_000,
        worktree_context_id,
    )?;

    assert_eq!(
        main_store.file_state_at("src/lib.rs", 3_000, main_context_id)?,
        Some("main worktree\n".to_string())
    );
    assert_eq!(
        worktree_store.file_state_at("src/lib.rs", 3_000, worktree_context_id)?,
        Some("feature worktree\n".to_string())
    );
    Ok(())
}

#[test]
fn new_projects_use_kairos_database_name() -> Result<()> {
    let temp = tempdir()?;
    let canonical_root = fs::canonicalize(temp.path())?;

    let paths = ProjectPaths::initialize(temp.path())?;

    assert_eq!(
        paths.database_path,
        canonical_root.join(".timeline/kairos.db")
    );
    Ok(())
}

#[test]
fn legacy_keiros_database_name_is_still_discovered() -> Result<()> {
    let temp = tempdir()?;
    let timeline_dir = temp.path().join(".timeline");
    fs::create_dir_all(&timeline_dir)?;
    fs::write(timeline_dir.join("keiros.db"), [])?;
    let canonical_root = fs::canonicalize(temp.path())?;

    let paths = ProjectPaths::discover(temp.path())?;

    assert_eq!(
        paths.database_path,
        canonical_root.join(".timeline/keiros.db")
    );
    Ok(())
}

fn open_store(root: &Path) -> Result<(ProjectPaths, TimelineStore)> {
    let paths = ProjectPaths::initialize(root)?;
    let store = TimelineStore::open(&paths, TimelineConfig::default())?;
    Ok((paths, store))
}

fn local_context_id(store: &mut TimelineStore, root: &Path, recorded_at_ms: i64) -> Result<i64> {
    let context = RuntimeContext::local(root)?;
    store.ensure_context(&context, recorded_at_ms)
}

fn current_context_id(store: &mut TimelineStore, root: &Path, recorded_at_ms: i64) -> Result<i64> {
    let context = detect_runtime_context(root)?;
    store.ensure_context(&context, recorded_at_ms)
}

fn write_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn init_git_repo(root: &Path) -> Result<()> {
    run_git(root, &["init"])?;
    run_git(root, &["config", "user.name", "Kairos Test"])?;
    run_git(root, &["config", "user.email", "kairos@example.com"])?;
    write_text(&root.join("README.md"), "seed\n")?;
    run_git(root, &["add", "README.md"])?;
    run_git(root, &["commit", "-m", "initial"])?;
    Ok(())
}

fn checkout_new_branch(root: &Path, branch: &str) -> Result<()> {
    run_git(root, &["checkout", "-b", branch])
}

fn run_git(root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn create_v1_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA user_version = 1;

        CREATE TABLE files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL UNIQUE,
            first_seen_ms INTEGER NOT NULL,
            last_seen_ms INTEGER NOT NULL
        );

        CREATE TABLE revisions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            ordinal INTEGER NOT NULL,
            recorded_at_ms INTEGER NOT NULL,
            event_type TEXT NOT NULL CHECK (event_type IN ('create', 'modify', 'delete')),
            storage_kind TEXT NOT NULL CHECK (storage_kind IN ('full', 'patch', 'none')),
            base_revision_id INTEGER REFERENCES revisions(id) ON DELETE SET NULL,
            content_hash TEXT,
            size_bytes INTEGER,
            payload BLOB,
            git_worktree_root TEXT,
            git_common_dir TEXT,
            git_branch_name TEXT,
            git_head_commit TEXT,
            git_detached_head INTEGER NOT NULL DEFAULT 0
        );

        CREATE UNIQUE INDEX idx_revisions_file_ordinal
            ON revisions (file_id, ordinal);
        CREATE INDEX idx_revisions_file_time
            ON revisions (file_id, recorded_at_ms DESC, id DESC);
        CREATE INDEX idx_revisions_time
            ON revisions (recorded_at_ms DESC, id DESC);

        CREATE TABLE watcher_state (
            slot INTEGER PRIMARY KEY CHECK (slot = 1),
            pid INTEGER NOT NULL,
            root_path TEXT NOT NULL,
            started_at_ms INTEGER NOT NULL,
            heartbeat_ms INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row?);
    }
    Ok(columns)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
    .context("failed to inspect sqlite tables")
}
