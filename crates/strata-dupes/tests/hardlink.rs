//! "Replace with hardlinks" on real files. Every Recycle Bin entry a test
//! creates is restored into its test folder (after removing the link that
//! took its name) and then deleted with the folder.

mod common;

use std::time::Duration;

use common::{TestDir, bytes, candidate, file_info};
use strata_clean::CancelToken;
use strata_clean::audit::MemoryAuditLog;
use strata_clean::consent::Prompt;
use strata_clean::recycle::{self, RestoreTicket};
use strata_core::Safety;
use strata_dupes::hardlink::{self, LinkConfig, LinkError};
use strata_dupes::*;

const MIB: usize = 1024 * 1024;

fn scan(paths: &[std::path::PathBuf]) -> DuplicateReport {
    let cfg = ScanConfig {
        min_size: 64 * 1024,
        ..ScanConfig::default()
    };
    match find_duplicates(
        paths.iter().map(|p| candidate(p)).collect::<Vec<_>>(),
        &cfg,
        &MemoryHashCache::new(),
        &CancelToken::new(),
        &|_| {},
    ) {
        ScanOutcome::Completed(r) => r,
        ScanOutcome::Cancelled(_) => panic!("cancelled"),
    }
}

/// Puts a replaced original back where it was, so the test folder holds
/// everything again and the Recycle Bin holds nothing of ours.
fn undo(ticket: &RestoreTicket) {
    let original = std::path::Path::new(&ticket.original_path);
    let _ = std::fs::remove_file(original);
    recycle::restore(ticket).expect("restore from Recycle Bin");
}

#[test]
fn copies_become_links_and_originals_are_recyclable() {
    let d = TestDir::new("link");
    let data = bytes(1, MIB + 11);
    let p = vec![
        d.file("keep.bin", &data),
        d.file(r"a\copy1.bin", &data),
        d.file(r"b\copy2.bin", &data),
    ];
    let r = scan(&p);
    assert_eq!(r.groups.len(), 1);
    let mut sel = Selection::new(&r);
    let keep_index = r.groups[0]
        .files
        .iter()
        .position(|f| f.path == p[0])
        .unwrap();
    sel.set_keeper(0, keep_index).unwrap();
    for i in 0..3 {
        if i != keep_index {
            sel.mark(0, i).unwrap();
        }
    }
    let action = hardlink::plan(&r, &sel, 0).unwrap();
    let consent = Prompt::new(action).confirm();
    let guard = common::guard();
    let mut audit = MemoryAuditLog::default();
    let out = hardlink::replace_with_hardlinks(
        &guard,
        consent,
        |_| Safety::Careful,
        &mut audit,
        &LinkConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(out.len(), 2);
    let (keep_id, _, links) = file_info(&p[0]);
    assert_eq!(links, 3);
    let mut tickets = Vec::new();
    for o in out {
        let ticket = o
            .result
            .unwrap_or_else(|e| panic!("{}: {e}", o.path.display()));
        let (id, _, _) = file_info(&o.path);
        assert_eq!(
            id,
            keep_id,
            "{} is not a link to the keeper",
            o.path.display()
        );
        assert_eq!(std::fs::read(&o.path).unwrap(), data);
        tickets.push(ticket);
    }
    // No temporary names left behind.
    for sub in ["a", "b"] {
        let names: Vec<_> = std::fs::read_dir(d.path.join(sub))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(names.len(), 1);
    }
    for t in &tickets {
        undo(t);
    }
    // The originals are back, as separate files again.
    assert_eq!(file_info(&p[0]).2, 1);
    assert_ne!(file_info(&p[1]).0, keep_id);
    assert_eq!(std::fs::read(&p[1]).unwrap(), data);
}

#[test]
fn a_copy_changed_after_the_scan_is_left_alone() {
    let d = TestDir::new("link-changed");
    let data = bytes(2, MIB + 7);
    let p = vec![d.file("keep.bin", &data), d.file("copy.bin", &data)];
    let r = scan(&p);
    let sel = Selection::all_but_suggested(&r);
    let action = hardlink::plan(&r, &sel, 0).unwrap();
    let victim = action.replace[0].1.path.clone();
    std::thread::sleep(Duration::from_millis(20));
    let other = bytes(3, MIB + 7);
    std::fs::write(&victim, &other).unwrap();
    let guard = common::guard();
    let mut audit = MemoryAuditLog::default();
    let out = hardlink::replace_with_hardlinks(
        &guard,
        Prompt::new(action).confirm(),
        |_| Safety::Probably,
        &mut audit,
        &LinkConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(matches!(
        out[0].result,
        Err(LinkError::Copy {
            reason: SkipReason::Changed
        })
    ));
    assert_eq!(std::fs::read(&victim).unwrap(), other);
    assert_eq!(std::fs::read_dir(&d.path).unwrap().count(), 2);
}

#[test]
fn a_changed_keeper_stops_everything() {
    let d = TestDir::new("link-keeper");
    let data = bytes(4, MIB + 3);
    let p = vec![
        d.file("a.bin", &data),
        d.file("b.bin", &data),
        d.file("c.bin", &data),
    ];
    let r = scan(&p);
    let sel = Selection::all_but_suggested(&r);
    let action = hardlink::plan(&r, &sel, 0).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&action.keeper.path, bytes(5, MIB + 3)).unwrap();
    let guard = common::guard();
    let out = hardlink::replace_with_hardlinks(
        &guard,
        Prompt::new(action).confirm(),
        |_| Safety::Probably,
        &mut MemoryAuditLog::default(),
        &LinkConfig::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(out.len(), 2);
    assert!(
        out.iter()
            .all(|o| matches!(o.result, Err(LinkError::Keeper { .. })))
    );
    for f in &p {
        assert_eq!(file_info(f).2, 1);
    }
}
