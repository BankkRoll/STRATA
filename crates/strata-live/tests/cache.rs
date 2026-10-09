//! Cache file policy: atomic writes, header checks, periodic and on-stop
//! saves, and catch-up from a saved cache on launch.

mod support;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use strata_index::cache::read_header;
use strata_live::{
    CacheFile, CachePolicy, Halt, IndexAccess, LiveEvent, LoadError, RescanReason, SourceError,
    Tailer, TailerConfig, index_position,
};
use support::{Harness, Model, ROOT_REC, Versions};

/// A scratch directory owned by one test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("strata-live-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn model() -> Model {
    let mut m = Model::new(Versions::V2);
    let a = m.create_in(ROOT_REC, true, 0, false);
    for i in 0..20 {
        m.create_in(a, i % 4 == 0, 1000 * i, false);
    }
    m
}

fn cfg(interval: Duration) -> TailerConfig {
    TailerConfig {
        cache: CachePolicy {
            interval,
            save_on_stop: true,
        },
        ..TailerConfig::default()
    }
}

#[test]
fn write_is_atomic_and_load_checks_the_volume() {
    let dir = Scratch::new("atomic");
    let cache = CacheFile::new(dir.file("vol.idx"));
    assert!(matches!(cache.load(0), Err(LoadError::Missing)));

    let mut index = model().build_index();
    let pos = strata_live::JournalPosition {
        journal_id: 42,
        usn: 4096,
    };
    cache.save(&mut index, pos).expect("save");
    assert!(!dir.file("vol.idx.tmp").exists(), "temporary file renamed");
    let loaded = cache.load(0).expect("load");
    assert_eq!(index_position(&loaded), pos);
    assert_eq!(loaded.len(), index.len());

    let err = cache.load(9).expect_err("other volume");
    assert_eq!(
        err.rescan_reason(),
        RescanReason::VolumeMismatch {
            expected: 9,
            found: 0
        }
    );

    // A truncated cache is rejected, never half-loaded.
    let bytes = std::fs::read(cache.path()).expect("read");
    std::fs::write(cache.path(), &bytes[..bytes.len() / 2]).expect("truncate");
    let err = cache.load(0).expect_err("truncated");
    assert!(matches!(err, LoadError::Unusable(_)));
    assert!(matches!(
        err.rescan_reason(),
        RescanReason::CacheUnusable(_)
    ));

    // Overwriting replaces the old file.
    cache.write(&bytes).expect("rewrite");
    assert_eq!(cache.load(0).expect("load").len(), index.len());
}

#[test]
fn periodic_save_records_the_applied_position() {
    let dir = Scratch::new("periodic");
    let path = dir.file("vol.idx");
    let mut h = Harness::new(model(), cfg(Duration::from_secs(10)), None);
    h.tailer = Tailer::start(h.cfg.clone(), &mut h.journal, h.tailer.position())
        .expect("start")
        .with_cache(CacheFile::new(&path));
    h.step().expect("first step");
    h.model.borrow_mut().create(3, false, 77, false);
    h.settle().expect("settle");
    assert!(!path.exists(), "no save before the interval");
    // With unsaved changes the read waits only until the save is due.
    assert!(h.journal.waits.last().expect("wait").is_some());

    h.tick(10_000).expect("save");
    let saved: Vec<_> = h
        .events
        .iter()
        .filter_map(|e| match e {
            LiveEvent::Saved(p) => Some(*p),
            _ => None,
        })
        .collect();
    assert_eq!(saved, vec![h.tailer.position()]);
    let header = read_header(&std::fs::read(&path).expect("cache")).expect("header");
    assert_eq!(header.usn_journal_id, h.model.borrow().journal_id);
    assert_eq!(header.last_usn, h.model.borrow().next_usn);

    // Saved and idle: the next read blocks without a timeout.
    h.step().expect("idle");
    assert_eq!(h.journal.waits.last(), Some(&None));
}

#[test]
fn stop_saves_and_launch_catches_up_from_the_cache() {
    let dir = Scratch::new("launch");
    let path = dir.file("vol.idx");
    let mut h = Harness::new(model(), cfg(Duration::from_secs(3600)), None);
    h.tailer = Tailer::start(h.cfg.clone(), &mut h.journal, h.tailer.position())
        .expect("start")
        .with_cache(CacheFile::new(&path));
    for k in 0..30 {
        let mut m = h.model.borrow_mut();
        m.create(k, k % 5 == 0, u32::from(k) * 100, false);
        m.rename(k * 3, k * 7, k % 2 == 0, false);
    }
    h.settle().expect("settle");

    // Shutdown: the blocked read is cancelled and the cache saved.
    h.model.borrow_mut().fail_read = Some(SourceError::Cancelled);
    let stop = AtomicBool::new(false);
    let mut events = Vec::new();
    let halt = h.tailer.run(
        &mut h.journal,
        &mut h.records,
        &mut h.index as &mut dyn IndexAccess,
        &h.clock,
        &stop,
        &mut |e| events.push(e),
    );
    assert_eq!(halt, Halt::Stopped);
    assert!(events.iter().any(|e| matches!(e, LiveEvent::Saved(_))));

    // While the app is closed the volume keeps changing.
    for k in 0..25 {
        let mut m = h.model.borrow_mut();
        m.delete(k * 11);
        m.write(k, 123_456, false, false);
        m.create(k, false, 9, false);
    }

    // Launch: load, validate, replay, then live.
    let index = CacheFile::new(&path).load(0).expect("load");
    h.index = index;
    h.tailer = Tailer::start(h.cfg.clone(), &mut h.journal, index_position(&h.index))
        .expect("resume from cache");
    assert!(matches!(
        h.tailer.status(),
        strata_live::LiveStatus::CatchingUp { .. }
    ));
    h.settle().expect("catch up");
    assert_eq!(h.tailer.status(), strata_live::LiveStatus::Live);
    h.assert_matches_fresh_scan();
}

#[test]
fn launch_after_a_wrap_needs_rescan() {
    let dir = Scratch::new("wrap");
    let path = dir.file("vol.idx");
    let mut h = Harness::new(model(), cfg(Duration::from_secs(3600)), None);
    let pos = h.tailer.position();
    CacheFile::new(&path).save(&mut h.index, pos).expect("save");
    {
        let mut m = h.model.borrow_mut();
        for k in 0..10 {
            m.create(k, false, 1, false);
        }
        let head = m.next_usn;
        m.purge_before(head);
    }
    let index = CacheFile::new(&path).load(0).expect("load");
    let err =
        Tailer::start(h.cfg.clone(), &mut h.journal, index_position(&index)).expect_err("wrapped");
    assert!(matches!(
        err,
        Halt::NeedsRescan(RescanReason::JournalWrapped { .. })
    ));
}
