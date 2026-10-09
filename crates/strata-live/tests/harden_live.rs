//! Journal failures in the middle of tailing, with changes read but not yet
//! applied: the journal disabled, its id changed, the volume dismounted
//! during a large burst, and a wrap between reads of one backlog. The
//! applied position never runs ahead of the index. Also: files written
//! long after the scan are not flagged as having future timestamps.

mod support;

use strata_core::EntryFlags;
use strata_live::{Halt, RescanReason, SourceError, StaleReason, TailerConfig};
use support::{Harness, Model, ROOT_REC, Versions, now};

fn harness(cfg: TailerConfig) -> (Harness, u64) {
    let mut m = Model::new(Versions::Mixed);
    let dir = m.create_in(ROOT_REC, true, 0, false);
    m.create_in(dir, false, 10, false);
    (Harness::new(m, cfg, None), dir)
}

/// Reads a batch of changes without applying them yet.
fn read_pending(h: &mut Harness, dir: u64, n: usize) {
    {
        let mut m = h.model.borrow_mut();
        for _ in 0..n {
            m.create_in(dir, false, 77, false);
        }
        m.close_all();
    }
    h.step().expect("read");
    assert!(h.tailer.pending() > 0, "changes are pending");
}

#[test]
fn journal_disabled_with_changes_pending_keeps_the_position_honest() {
    let (mut h, dir) = harness(TailerConfig::default());
    let before = h.tailer.position();
    read_pending(&mut h, dir, 20);
    h.model.borrow_mut().disabled = true;
    // More changes arrive; the read fails.
    h.model.borrow_mut().create_in(dir, false, 1, false);
    let halt = loop {
        match h.tick(1000) {
            Ok(()) => {}
            Err(e) => break e,
        }
    };
    assert_eq!(halt, Halt::JournalDisabled);
    h.index.check_invariants().unwrap();
    let pos = h.tailer.position();
    assert_eq!(pos.journal_id, before.journal_id);
    assert!(pos.usn <= h.tailer.read_position());
    // Whatever was applied is a prefix of the journal: replaying from the
    // position after re-enabling converges on a fresh scan.
    h.model.borrow_mut().disabled = false;
    h.tailer.resume(&mut h.journal).expect("resume");
    h.settle().expect("catch up");
    h.assert_matches_fresh_scan();
}

#[test]
fn journal_recreated_with_changes_pending_needs_rescan() {
    let (mut h, dir) = harness(TailerConfig::default());
    read_pending(&mut h, dir, 20);
    h.model.borrow_mut().recreate_journal();
    h.model.borrow_mut().create_in(dir, false, 1, false);
    let halt = loop {
        match h.tick(1000) {
            Ok(()) => {}
            Err(e) => break e,
        }
    };
    assert!(
        matches!(
            halt,
            Halt::NeedsRescan(RescanReason::JournalIdChanged { .. })
        ),
        "{halt:?}"
    );
    h.index.check_invariants().unwrap();
}

#[test]
fn volume_dismounted_mid_burst_resumes_to_a_fresh_scan() {
    let cfg = TailerConfig {
        max_refresh_per_tick: 500,
        fetch_batch: 100,
        ..TailerConfig::default()
    };
    let (mut h, dir) = harness(cfg);
    {
        let mut m = h.model.borrow_mut();
        for i in 0..5000u32 {
            let f = m.create_in(dir, false, i, false);
            if i % 3 == 0 {
                m.write_rec(f, i + 1, false, false);
            }
        }
        m.close_all();
    }
    // Apply part of the burst, then lose the volume during a fetch.
    for _ in 0..4 {
        h.tick(1000).expect("partial progress");
    }
    let applied_before = h.tailer.position();
    h.model.borrow_mut().fail_fetch = Some(SourceError::VolumeGone);
    let halt = loop {
        match h.tick(1000) {
            Ok(()) => {}
            Err(e) => break e,
        }
    };
    assert_eq!(halt, Halt::Stale(StaleReason::VolumeGone));
    assert!(h.tailer.position().usn >= applied_before.usn);
    h.index.check_invariants().unwrap();

    // Remounted; the same journal continues.
    h.tailer.resume(&mut h.journal).expect("resume");
    h.settle().expect("catch up");
    h.assert_matches_fresh_scan();
}

#[test]
fn wrap_between_reads_of_one_backlog_needs_rescan() {
    let (mut h, dir) = harness(TailerConfig::default());
    h.journal.max_bytes.set(512);
    {
        let mut m = h.model.borrow_mut();
        for _ in 0..200 {
            m.create_in(dir, false, 3, false);
        }
    }
    h.step().expect("first read");
    let read = h.tailer.read_position();
    assert!(read < h.model.borrow().next_usn, "backlog left to read");
    {
        let mut m = h.model.borrow_mut();
        let head = m.next_usn;
        m.purge_before(head);
    }
    let halt = loop {
        match h.tick(1000) {
            Ok(()) => {}
            Err(e) => break e,
        }
    };
    assert!(
        matches!(halt, Halt::NeedsRescan(RescanReason::JournalWrapped { .. })),
        "{halt:?}"
    );
    h.index.check_invariants().unwrap();
}

#[test]
fn files_written_long_after_the_scan_are_not_suspicious() {
    let (mut h, dir) = harness(TailerConfig::default());
    let scan_time = now().to_unix_secs();
    let recent = {
        let mut m = h.model.borrow_mut();
        let f = m.create_in(dir, false, 5, false);
        // Written a week after the index was built, but in the past.
        m.nodes.get_mut(&f).unwrap().mtime = scan_time + 7 * 86_400;
        f
    };
    let future = {
        let mut m = h.model.borrow_mut();
        let f = m.create_in(dir, false, 5, false);
        m.nodes.get_mut(&f).unwrap().mtime = 4_102_444_800 + 365 * 86_400; // 2101
        f
    };
    h.settle().expect("settle");
    let flags = |rec: u64| {
        let r = h.model.borrow().file_ref(rec);
        h.index.flags(h.index.lookup(r).unwrap())
    };
    assert!(!flags(recent).contains(EntryFlags::SUSPICIOUS_TIME));
    assert!(flags(future).contains(EntryFlags::SUSPICIOUS_TIME));
}
