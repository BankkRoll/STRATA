//! Journal edge cases: wrap, id change, disabled journal, disconnects and
//! resume, unsupported ids, malformed buffers, split renames, coalescing,
//! irrelevant reasons, never-seen deletes, root protection and blocking
//! waits.

mod support;

use std::time::Duration;

use strata_core::FileRef;
use strata_live::{
    Halt, JournalPosition, LiveEvent, RescanReason, SourceError, StaleReason, Tailer, TailerConfig,
};
use strata_ntfs::usn::{self, FileId128, UsnChange, encode};
use support::{FakeJournal, Harness, Model, ROOT_REC, Versions};

/// Root with directories `a` and `b` and a file in `a`.
fn base() -> (Model, u64, u64, u64) {
    let mut m = Model::new(Versions::Mixed);
    let a = m.create_in(ROOT_REC, true, 0, false);
    let b = m.create_in(ROOT_REC, true, 0, false);
    let f = m.create_in(a, false, 5000, false);
    (m, a, b, f)
}

fn harness() -> (Harness, u64, u64, u64) {
    let (m, a, b, f) = base();
    (Harness::new(m, TailerConfig::default(), None), a, b, f)
}

fn start_err(m: Model, pos: JournalPosition) -> Halt {
    let mut j = FakeJournal::new(std::rc::Rc::new(std::cell::RefCell::new(m)));
    Tailer::start(TailerConfig::default(), &mut j, pos).expect_err("start must fail")
}

// -----------------------------------------------------------------------------
// Journal state
// -----------------------------------------------------------------------------

#[test]
fn wrapped_journal_at_start_needs_rescan() {
    let (mut m, ..) = base();
    let id = m.journal_id;
    m.create(0, false, 10, false);
    let head = m.next_usn;
    m.purge_before(head);
    let h = start_err(
        m,
        JournalPosition {
            journal_id: id,
            usn: 0,
        },
    );
    assert_eq!(
        h,
        Halt::NeedsRescan(RescanReason::JournalWrapped {
            saved_usn: 0,
            first_usn: head
        })
    );
}

#[test]
fn journal_wrapping_while_tailing_needs_rescan() {
    let (mut h, ..) = harness();
    let pos = h.tailer.position();
    {
        let mut m = h.model.borrow_mut();
        for _ in 0..5 {
            m.create(0, false, 10, false);
        }
        let head = m.next_usn;
        m.purge_before(head);
    }
    let head = h.model.borrow().next_usn;
    assert_eq!(
        h.step(),
        Err(Halt::NeedsRescan(RescanReason::JournalWrapped {
            saved_usn: pos.usn,
            first_usn: head
        }))
    );
}

#[test]
fn journal_id_change_needs_rescan() {
    let (mut m, ..) = base();
    let old = m.journal_id;
    m.recreate_journal();
    let new = m.journal_id;
    let pos = JournalPosition {
        journal_id: old,
        usn: m.next_usn,
    };
    assert_eq!(
        start_err(m, pos),
        Halt::NeedsRescan(RescanReason::JournalIdChanged {
            expected: old,
            found: Some(new)
        })
    );

    let (mut h, ..) = harness();
    h.model.borrow_mut().recreate_journal();
    let found = h.model.borrow().journal_id;
    assert_eq!(
        h.step(),
        Err(Halt::NeedsRescan(RescanReason::JournalIdChanged {
            expected: found - 1,
            found: Some(found)
        }))
    );
}

#[test]
fn disabled_journal_is_a_typed_state() {
    let (mut m, ..) = base();
    let pos = JournalPosition {
        journal_id: m.journal_id,
        usn: m.next_usn,
    };
    m.disabled = true;
    assert_eq!(start_err(m, pos), Halt::JournalDisabled);

    let (mut h, ..) = harness();
    h.model.borrow_mut().disabled = true;
    assert_eq!(h.step(), Err(Halt::JournalDisabled));
}

#[test]
fn position_past_the_end_needs_rescan() {
    let (m, ..) = base();
    let pos = JournalPosition {
        journal_id: m.journal_id,
        usn: m.next_usn + 8,
    };
    assert!(matches!(
        start_err(m, pos),
        Halt::NeedsRescan(RescanReason::UsnAhead { .. })
    ));
}

// -----------------------------------------------------------------------------
// Disconnects, dismounts, sleep/resume
// -----------------------------------------------------------------------------

#[test]
fn disconnect_marks_stale_and_resume_catches_up() {
    let (mut h, a, _, f) = harness();
    let before = h.tailer.position();
    {
        let mut m = h.model.borrow_mut();
        m.create_in(a, false, 1234, false);
        m.write_rec(f, 99_999, false, false);
    }
    h.step().expect("read");
    h.model.borrow_mut().fail_fetch = Some(SourceError::Disconnected);
    assert_eq!(h.tick(300), Err(Halt::Stale(StaleReason::Disconnected)));
    // Nothing that was read but not applied is covered by the position.
    assert_eq!(h.tailer.position(), before);
    h.index.check_invariants().expect("index intact");

    // Changes keep happening while the machine sleeps.
    h.model.borrow_mut().create_in(a, true, 0, false);
    h.model.borrow_mut().fail_read = Some(SourceError::VolumeGone);
    assert_eq!(h.step(), Err(Halt::Stale(StaleReason::VolumeGone)));

    h.tailer.resume(&mut h.journal).expect("resume");
    h.settle().expect("catch up");
    h.assert_matches_fresh_scan();
}

#[test]
fn resume_after_a_wrap_needs_rescan() {
    let (mut h, ..) = harness();
    h.model.borrow_mut().fail_read = Some(SourceError::Disconnected);
    assert!(matches!(h.step(), Err(Halt::Stale(_))));
    {
        let mut m = h.model.borrow_mut();
        m.create(0, false, 1, false);
        let head = m.next_usn;
        m.purge_before(head);
    }
    assert!(matches!(
        h.tailer.resume(&mut h.journal),
        Err(Halt::NeedsRescan(RescanReason::JournalWrapped { .. }))
    ));
}

#[test]
fn cancelled_read_stops() {
    let (mut h, ..) = harness();
    h.model.borrow_mut().fail_read = Some(SourceError::Cancelled);
    assert_eq!(h.step(), Err(Halt::Stopped));
}

// -----------------------------------------------------------------------------
// Malformed or unsupported input
// -----------------------------------------------------------------------------

#[test]
fn wide_file_ids_are_unsupported() {
    let (mut h, ..) = harness();
    let c = UsnChange {
        major_version: 3,
        file: FileId128(1 << 80 | 77),
        parent: FileId128(5),
        usn: h.model.borrow().next_usn,
        timestamp: strata_core::FileTime(0),
        reason: usn::USN_REASON_FILE_CREATE,
        source_info: 0,
        security_id: 0,
        attributes: 0,
        name: strata_core::WideName::from_str_lossless("refs"),
    };
    h.model.borrow_mut().push_bytes(encode::change(&c));
    assert_eq!(
        h.step(),
        Err(Halt::NeedsRescan(RescanReason::UnsupportedFileIds))
    );
}

#[test]
fn malformed_buffer_needs_rescan() {
    let (mut h, ..) = harness();
    let mut junk = vec![0u8; 64];
    junk[0..4].copy_from_slice(&64u32.to_le_bytes());
    junk[4..6].copy_from_slice(&9u16.to_le_bytes());
    h.model.borrow_mut().push_bytes(junk);
    assert!(matches!(
        h.step(),
        Err(Halt::NeedsRescan(RescanReason::MalformedJournal(_)))
    ));
}

// -----------------------------------------------------------------------------
// Applying
// -----------------------------------------------------------------------------

#[test]
fn rename_split_across_reads_is_applied_once() {
    let (mut h, a, b, f) = harness();
    let fref = h.model.borrow().file_ref(f);
    let first = h.model.borrow().journal.len();
    h.model.borrow_mut().rename_link(f, 0, b, false);
    // Only the old-name half has been written when the tailer reads.
    let new_half = h.model.borrow().journal[first + 1].0;
    h.journal.hide_from = Some(new_half);
    h.step().expect("read old half");
    h.tick(300).expect("flush");
    assert!(
        !h.model.borrow().fetch_log.contains(&fref),
        "a half-renamed file is held back"
    );

    h.journal.hide_from = None;
    h.tick(300).expect("read new half and flush");
    let fetched = h
        .model
        .borrow()
        .fetch_log
        .iter()
        .filter(|&&r| r == fref)
        .count();
    assert_eq!(fetched, 1);
    assert_eq!(h.tailer.stats().paired_renames, 1);

    let id = h.index.lookup(fref).expect("file");
    let (ia, ib) = {
        let m = h.model.borrow();
        (
            h.index.lookup(m.file_ref(a)).expect("a"),
            h.index.lookup(m.file_ref(b)).expect("b"),
        )
    };
    let tick = h
        .ticks()
        .into_iter()
        .find(|t| t.changes.updated.contains(&id))
        .expect("tick");
    let aggs: Vec<_> = tick.changes.aggregates.iter().map(|(d, _)| *d).collect();
    assert!(
        aggs.contains(&ia) && aggs.contains(&ib),
        "both parents' totals change"
    );
    h.settle().expect("settle");
    h.assert_matches_fresh_scan();
}

#[test]
fn ten_thousand_writes_are_one_refresh() {
    let (mut h, _, _, f) = harness();
    let fref = h.model.borrow().file_ref(f);
    for i in 0..10_000u32 {
        h.model.borrow_mut().write_rec(f, 5000 + i, false, false);
    }
    let head = h.model.borrow().next_usn;
    while h.tailer.read_position() < head {
        h.step().expect("read");
    }
    h.tick(300).expect("flush");
    let m = h.model.borrow();
    assert_eq!(m.fetch_log.iter().filter(|&&r| r == fref).count(), 1);
    assert_eq!(h.tailer.stats().records, 20_000);
    drop(m);
    h.settle().expect("settle");
    h.assert_matches_fresh_scan();
}

#[test]
fn irrelevant_reasons_fetch_nothing() {
    let (mut h, ..) = harness();
    for k in 0..4 {
        h.model.borrow_mut().security(k);
    }
    h.settle().expect("settle");
    assert_eq!(h.model.borrow().fetch_calls, 0);
    assert!(h.ticks().is_empty());
    assert_eq!(h.tailer.stats().ignored, 8);
    assert_eq!(h.tailer.position().usn, h.model.borrow().next_usn);
}

#[test]
fn deletes_of_never_seen_refs_are_harmless() {
    let (mut h, ..) = harness();
    h.model.borrow_mut().ghost_delete(7);
    h.settle().expect("settle");
    let ghost = FileRef::from_parts(1_000_007, 3);
    assert!(!h.model.borrow().fetch_log.contains(&ghost));
    h.assert_matches_fresh_scan();
}

#[test]
fn duplicate_and_late_closes_are_harmless() {
    let (mut h, _, _, f) = harness();
    h.model.borrow_mut().write_rec(f, 1, true, false);
    h.tick(300).expect("tick");
    // Writes inside the open handle are not journaled until the close.
    h.model.borrow_mut().write_rec(f, 777_777, true, false);
    h.tick(300).expect("tick");
    for k in 0..3 {
        h.model.borrow_mut().dup_close(k);
    }
    h.settle().expect("settle");
    h.assert_matches_fresh_scan();
}

#[test]
fn root_is_never_removed() {
    let (mut h, ..) = harness();
    let root = h.model.borrow().file_ref(ROOT_REC);
    h.model
        .borrow_mut()
        .emit_raw(root, usn::USN_REASON_FILE_DELETE | usn::USN_REASON_CLOSE);
    h.settle().expect("settle");
    assert!(h.ticks().iter().any(|t| t.skipped == 1));
    h.assert_matches_fresh_scan();
}

// -----------------------------------------------------------------------------
// Blocking and latency
// -----------------------------------------------------------------------------

#[test]
fn idle_reads_block_and_ticks_wait_for_the_deadline() {
    let (mut h, a, ..) = harness();
    h.step().expect("idle");
    assert_eq!(h.journal.waits.last(), Some(&None), "idle read blocks");

    h.model.borrow_mut().create_in(a, false, 10, false);
    h.step().expect("read");
    assert!(h.ticks().is_empty(), "nothing applied before the tick");
    h.step().expect("wait");
    assert_eq!(
        h.journal.waits.last(),
        Some(&Some(Duration::from_millis(250))),
        "pending work waits only until the tick deadline"
    );
    h.tick(250).expect("flush");
    assert_eq!(h.ticks().len(), 1, "applied exactly at the deadline");
    h.step().expect("idle again");
    assert_eq!(h.journal.waits.last(), Some(&None));
    assert!(
        h.events.iter().all(|e| !matches!(e, LiveEvent::Status(_))),
        "a single change does not leave the live state"
    );
}
