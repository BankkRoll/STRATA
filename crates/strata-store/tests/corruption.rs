//! Corruption detection and recovery: a damaged database must never panic or
//! take the other database down with it.

mod common;

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use common::*;
use strata_store::*;

fn reopen(dir: &Path) -> Store {
    Store::open_with_clock(dir, Arc::new(ManualClock::new(t0()))).unwrap()
}

fn seeded_store() -> (tempfile::TempDir, Store) {
    let (d, store, _c) = store_at(t0());
    snap(&store, GIB, synthetic_dirs(2000, 3));
    let mut s = store.load_settings().unwrap();
    s.appearance.theme = Theme::Dark;
    store.save_settings(&s).unwrap();
    (d, store)
}

fn assert_settings_survived(store: &Store) {
    assert_eq!(store.health().state, DbHealth::Ok);
    assert_eq!(store.load_settings().unwrap().appearance.theme, Theme::Dark);
}

#[test]
fn garbage_file_is_reported_and_reset() {
    let (d, store) = seeded_store();
    drop(store);
    std::fs::write(d.path().join("history.db"), vec![0xA5u8; 64 * 1024]).unwrap();
    // A WAL left from the old file must not be replayed onto the garbage.
    let _ = std::fs::remove_file(d.path().join("history.db-wal"));

    let store = reopen(d.path());
    assert!(matches!(store.health().history, DbHealth::Corrupt { .. }));
    assert!(matches!(
        store.snapshots(&volume()),
        Err(StoreError::Corrupt {
            db: DbKind::History,
            ..
        })
    ));
    assert!(matches!(
        store.record_activity(&[]),
        Err(StoreError::Corrupt { .. })
    ));
    assert_settings_survived(&store);

    let report = store.reset_history().unwrap();
    let moved = report.moved_to.unwrap();
    assert!(moved.exists());
    let name = moved.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name, "history.db.corrupt-20261009T120000Z");
    assert_eq!(std::fs::read(&moved).unwrap(), vec![0xA5u8; 64 * 1024]);

    assert_eq!(store.health().history, DbHealth::Ok);
    assert!(store.snapshots(&volume()).unwrap().is_empty());
    snap(&store, GIB, vec![dir(r"C:\A", GIB)]);
    assert_settings_survived(&store);
}

#[test]
fn damaged_pages_fail_quick_check() {
    let (d, store) = seeded_store();
    drop(store);
    let path = d.path().join("history.db");
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len > 3 * 4096, "fixture too small: {len}");
    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    // Keep the 100-byte header intact so SQLite recognizes the file, and
    // trash the b-tree pages behind it.
    f.seek(SeekFrom::Start(4096)).unwrap();
    f.write_all(&vec![0xEE; (len - 4096) as usize]).unwrap();
    drop(f);

    let store = reopen(d.path());
    match store.health().history {
        DbHealth::Corrupt { detail } => assert!(!detail.is_empty()),
        other => panic!("expected corrupt, got {other:?}"),
    }
    assert_settings_survived(&store);
    store.reset_history().unwrap();
    assert_eq!(store.health().history, DbHealth::Ok);
}

#[test]
fn corrupt_state_leaves_history_working() {
    let (d, store) = seeded_store();
    drop(store);
    std::fs::write(
        d.path().join("state.db"),
        b"not a database at all, honestly",
    )
    .unwrap();
    let _ = std::fs::remove_file(d.path().join("state.db-wal"));

    let store = reopen(d.path());
    assert!(matches!(store.health().state, DbHealth::Corrupt { .. }));
    assert_eq!(store.health().history, DbHealth::Ok);
    assert_eq!(store.snapshots(&volume()).unwrap().len(), 1);
    assert!(matches!(
        store.load_settings(),
        Err(StoreError::Corrupt {
            db: DbKind::State,
            ..
        })
    ));
    assert!(store.reset_state().unwrap().moved_to.is_some());
    assert_eq!(store.load_settings().unwrap(), Settings::default());
}

#[test]
fn repeated_resets_get_unique_names() {
    let (d, store, _c) = store_at(t0());
    let first = store.reset_history().unwrap().moved_to.unwrap();
    let second = store.reset_history().unwrap().moved_to.unwrap();
    assert_ne!(first, second);
    assert!(first.exists() && second.exists());
    assert!(d.path().join("history.db").exists());
}

#[test]
fn too_new_history_can_be_reset() {
    let (d, store, _c) = store_at(t0());
    drop(store);
    {
        let conn = rusqlite::Connection::open(d.path().join("history.db")).unwrap();
        conn.pragma_update(None, "user_version", 1000).unwrap();
    }
    let store = reopen(d.path());
    assert!(matches!(
        store.health().history,
        DbHealth::TooNew { found: 1000, .. }
    ));
    assert!(matches!(
        store.volumes(),
        Err(StoreError::TooNew { found: 1000, .. })
    ));
    store.reset_history().unwrap();
    assert_eq!(store.health().history, DbHealth::Ok);
}

#[test]
fn damaged_snapshot_blob_is_a_typed_error() {
    let (d, store, _c) = store_at(t0());
    let id = snap(&store, GIB, vec![dir(r"C:\A", GIB)]);
    {
        let conn = rusqlite::Connection::open(d.path().join("history.db")).unwrap();
        conn.execute(
            "UPDATE snapshot_dirs SET data = x'FFFFFFFF' WHERE snapshot_id = ?1",
            [id.0],
        )
        .unwrap();
    }
    assert!(matches!(
        store.snapshot_dir(id, r"C:\A"),
        Err(StoreError::Corrupt { .. })
    ));
    assert!(matches!(
        store.diff(id, id, &DiffOptions::default()),
        Err(StoreError::Corrupt { .. })
    ));
}

#[test]
fn unwritable_directory_is_an_io_error() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("plain-file");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(
        Store::open(&file.join("sub")),
        Err(StoreError::Io { .. })
    ));
}
