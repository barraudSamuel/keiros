use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::Result;
use tempfile::tempdir;

use keiros::{
    config::{ProjectPaths, TimelineConfig, DAY_MS, DEFAULT_MAX_FILE_SIZE_BYTES},
    debounce::Debouncer,
    filter::ProjectFilter,
    restore::{restore_file, restore_project},
    store::TimelineStore,
};

#[test]
fn debounce_waits_for_stable_idle_window() {
    let base = Instant::now();
    let path = Path::new("src/lib.rs").to_path_buf();
    let mut debouncer = Debouncer::new(Duration::from_millis(1_000));

    debouncer.record(path.clone(), base);
    assert!(debouncer.drain_ready(base + Duration::from_millis(500)).is_empty());

    debouncer.record(path.clone(), base + Duration::from_millis(700));
    assert!(debouncer.drain_ready(base + Duration::from_millis(1_500)).is_empty());

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
    write_text(&temp.path().join("node_modules/pkg/index.js"), "module.exports = {}\n")?;

    let filter = ProjectFilter::new(temp.path().to_path_buf(), DEFAULT_MAX_FILE_SIZE_BYTES)?;
    assert!(filter.read_trackable_text(&temp.path().join("src/lib.rs"))?.is_some());
    assert!(filter.read_trackable_text(&temp.path().join("ignored.rs"))?.is_none());
    assert!(filter.read_trackable_text(&temp.path().join(".env"))?.is_none());
    assert!(
        filter
            .read_trackable_text(&temp.path().join("node_modules/pkg/index.js"))?
            .is_none()
    );
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

    let (paths, mut store) = open_store(temp.path())?;
    store.record_text("src/main.rs", "fn main() { println!(\"v1\"); }\n", 1_000)?;
    store.record_text("src/main.rs", "fn main() { println!(\"v2\"); }\n", 2_000)?;

    restore_file(&paths.root, &store, Path::new("src/main.rs"), 1_500)?;
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

    let (paths, mut store) = open_store(temp.path())?;
    store.record_text("src/a.rs", "a1\n", 1_000)?;
    store.record_text("src/b.rs", "b1\n", 1_000)?;
    store.record_text("src/a.rs", "a2\n", 2_000)?;
    store.record_delete("src/b.rs", 3_000)?;
    store.record_text("src/c.rs", "c1\n", 3_000)?;

    let summary = restore_project(&paths.root, &store, 1_500)?;
    assert_eq!(summary.restored_files, 2);
    assert_eq!(summary.removed_files, 1);
    assert_eq!(fs::read_to_string(temp.path().join("src/a.rs"))?, "a1\n");
    assert_eq!(fs::read_to_string(temp.path().join("src/b.rs"))?, "b1\n");
    assert!(!temp.path().join("src/c.rs").exists());
    Ok(())
}

#[test]
fn delete_revisions_make_the_file_absent_after_the_delete_timestamp() -> Result<()> {
    let temp = tempdir()?;
    let (_paths, mut store) = open_store(temp.path())?;
    store.record_text("src/lib.rs", "v1\n", 1_000)?;
    store.record_delete("src/lib.rs", 2_000)?;

    assert_eq!(store.file_state_at("src/lib.rs", 1_500)?, Some("v1\n".to_string()));
    assert_eq!(store.file_state_at("src/lib.rs", 2_500)?, None);
    Ok(())
}

#[test]
fn pruning_keeps_a_boundary_anchor_and_newer_revisions() -> Result<()> {
    let temp = tempdir()?;
    let (_paths, mut store) = open_store(temp.path())?;
    store.record_text("src/lib.rs", "v0\n", 0)?;
    store.record_text("src/lib.rs", "v1\n", DAY_MS)?;
    store.record_text("src/lib.rs", "v2\n", 8 * DAY_MS)?;

    let summary = store.prune(10 * DAY_MS)?;
    assert_eq!(summary.removed_revisions, 1);
    assert_eq!(store.file_history("src/lib.rs")?.len(), 2);
    assert_eq!(store.file_state_at("src/lib.rs", 4 * DAY_MS)?, Some("v1\n".to_string()));
    assert_eq!(store.file_state_at("src/lib.rs", 8 * DAY_MS)?, Some("v2\n".to_string()));
    Ok(())
}

#[test]
fn timestamp_lookup_uses_latest_revision_at_or_before_the_target() -> Result<()> {
    let temp = tempdir()?;
    let (_paths, mut store) = open_store(temp.path())?;
    store.record_text("src/lib.rs", "v1\n", 1_000)?;
    store.record_text("src/lib.rs", "v2\n", 2_000)?;
    store.record_delete("src/lib.rs", 3_000)?;
    store.record_text("src/lib.rs", "v3\n", 4_000)?;

    assert_eq!(store.file_state_at("src/lib.rs", 1_999)?, Some("v1\n".to_string()));
    assert_eq!(store.file_state_at("src/lib.rs", 2_500)?, Some("v2\n".to_string()));
    assert_eq!(store.file_state_at("src/lib.rs", 3_500)?, None);
    assert_eq!(store.file_state_at("src/lib.rs", 4_500)?, Some("v3\n".to_string()));
    Ok(())
}

fn open_store(root: &Path) -> Result<(ProjectPaths, TimelineStore)> {
    let paths = ProjectPaths::initialize(root)?;
    let store = TimelineStore::open(&paths, TimelineConfig::default())?;
    Ok((paths, store))
}

fn write_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}
