//! End-to-end pipeline tests on real files.

mod common;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{TestDir, bytes, candidate, file_info, set_attributes, volume};
use strata_clean::CancelToken;
use strata_core::{CloudState, EntryFlags, FileRef};
use strata_dupes::*;

const MIB: usize = 1024 * 1024;

fn cfg() -> ScanConfig {
    ScanConfig {
        min_size: 64 * 1024,
        concurrency: 2,
        progress_interval: Duration::ZERO,
        cache_batch: 1,
        ..ScanConfig::default()
    }
}

fn run(cands: Vec<Candidate>, cfg: &ScanConfig, cache: &dyn HashCache) -> DuplicateReport {
    match find_duplicates(cands, cfg, cache, &CancelToken::new(), &|_| {}) {
        ScanOutcome::Completed(r) => r,
        ScanOutcome::Cancelled(_) => panic!("unexpectedly cancelled"),
    }
}

#[test]
fn identical_files_form_groups_sorted_by_wasted_bytes() {
    let d = TestDir::new("groups");
    let a = bytes(1, 2 * MIB);
    let b = bytes(2, 3 * MIB / 2);
    let paths = [
        d.file("a1.bin", &a),
        d.file("x/a2.bin", &a),
        d.file("y/a3.bin", &a),
        d.file("b1.bin", &b),
        d.file("z/b2.bin", &b),
        d.file("unique.bin", &bytes(3, 2 * MIB)),
    ];
    let r = run(
        paths.iter().map(|p| candidate(p)).collect(),
        &cfg(),
        &MemoryHashCache::new(),
    );
    assert_eq!(r.groups.len(), 2);
    assert_eq!(r.groups[0].files.len(), 3);
    assert_eq!(r.groups[0].wasted_bytes(), 4 * MIB as u64);
    assert_eq!(r.groups[1].files.len(), 2);
    assert_eq!(r.groups[1].wasted_bytes(), 3 * MIB as u64 / 2);
    assert_eq!(r.groups[0].hash, *blake3::hash(&a).as_bytes());
    assert_eq!(r.stats.wasted_bytes, 4 * MIB as u64 + 3 * MIB as u64 / 2);
    assert_eq!(r.stats.groups, 2);
    assert_eq!(r.stats.duplicate_files, 5);
    for (i, g) in r.groups.iter().enumerate() {
        assert_eq!(g.id, i as u64);
        assert!(g.keep.index < g.files.len());
        for f in &g.files {
            let (id, _, _) = file_info(&f.path);
            assert_eq!(f.file_ref, FileRef(id));
        }
    }
    // The shortest path wins among same-age copies in plain folders.
    let keeper = &r.groups[1].files[r.groups[1].keep.index];
    assert!(keeper.path.ends_with("b1.bin"), "{keeper:?}");
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
}

#[test]
fn same_size_different_content_is_not_a_duplicate() {
    let d = TestDir::new("collide");
    let size = 2 * MIB;
    let base = bytes(10, size);
    // Same first and last 64 KiB, different middle: the partial hash splits.
    let mut mid = base.clone();
    mid[size / 2] ^= 0xFF;
    // Same first, middle and last windows, different elsewhere: only the
    // full hash splits.
    let mut far = base.clone();
    far[300 * 1024] ^= 0xFF;
    let p = [
        d.file("base.bin", &base),
        d.file("mid.bin", &mid),
        d.file("far.bin", &far),
    ];
    let r = run(
        p.iter().map(|p| candidate(p)).collect(),
        &cfg(),
        &MemoryHashCache::new(),
    );
    assert!(r.groups.is_empty());
    assert_eq!(r.stats.partial_hashed, 3);
    // `mid` dropped out after the partial hash; `base` and `far` collided.
    assert_eq!(r.stats.full_hashed, 2);
}

#[test]
fn hardlinks_are_one_file() {
    let d = TestDir::new("hardlink");
    let data = bytes(20, MIB + 17);
    let h = d.file("h.bin", &data);
    let h2 = d.path.join("h2.bin");
    std::fs::hard_link(&h, &h2).unwrap();
    let copy = d.file("copy.bin", &data);

    // As the index reports it: the second link is flagged secondary.
    let mut secondary = candidate(&h2);
    secondary.flags |= EntryFlags::HARDLINK_SECONDARY;
    let r = run(
        vec![candidate(&h), secondary, candidate(&copy)],
        &cfg(),
        &MemoryHashCache::new(),
    );
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].files.len(), 2);
    assert_eq!(r.stats.excluded[&Exclusion::HardlinkSecondary], 1);

    // Without the flag, the shared file reference still makes it one file.
    let r = run(
        vec![candidate(&h), candidate(&h2)],
        &cfg(),
        &MemoryHashCache::new(),
    );
    assert!(r.groups.is_empty());
    assert_eq!(r.stats.excluded[&Exclusion::SameFile], 1);
}

#[test]
fn alternate_streams_are_ignored() {
    let d = TestDir::new("ads");
    let data = bytes(30, MIB + 5);
    let a = d.file("a.bin", &data);
    let b = d.file("b.bin", &data);
    let mut ads = a.clone().into_os_string();
    ads.push(":Zone.Identifier");
    std::fs::write(&ads, bytes(31, 4096)).unwrap();
    // The index reports all streams in its size and flags the ADS.
    let mut ca = candidate(&a);
    ca.size += 4096;
    ca.flags |= EntryFlags::HAS_ADS;
    let r = run(vec![ca, candidate(&b)], &cfg(), &MemoryHashCache::new());
    assert_eq!(r.groups.len(), 1, "{:?}", r.stats);
    assert_eq!(r.groups[0].size, data.len() as u64);
    // The stream is untouched.
    assert_eq!(std::fs::read(&ads).unwrap().len(), 4096);
}

#[test]
fn empty_and_small_files_are_excluded() {
    let d = TestDir::new("small");
    let small = bytes(40, 1000);
    let p = [
        d.file("e1", b""),
        d.file("e2", b""),
        d.file("s1", &small),
        d.file("s2", &small),
    ];
    let r = run(
        p.iter().map(|p| candidate(p)).collect(),
        &cfg(),
        &MemoryHashCache::new(),
    );
    assert!(r.groups.is_empty());
    assert_eq!(r.stats.excluded[&Exclusion::Empty], 2);
    assert_eq!(r.stats.excluded[&Exclusion::BelowMinSize], 2);
    assert_eq!(r.stats.measured, 0, "nothing below the minimum is opened");
}

#[test]
fn file_changed_between_partial_and_full_hash_is_dropped() {
    let d = TestDir::new("changed");
    let data = bytes(50, 2 * MIB);
    let p = [
        d.file("c1.bin", &data),
        d.file("c2.bin", &data),
        d.file("c3.bin", &data),
    ];
    let victim = p[2].clone();
    let cache = MemoryHashCache::new();
    let done = AtomicBool::new(false);
    // NOTE: one hashing thread makes the rewrite below land before the victim
    // (hashed last) is read; with a pool, another thread can finish it first.
    let cfg = ScanConfig {
        concurrency: 1,
        ..cfg()
    };
    let out = find_duplicates(
        p.iter().map(|p| candidate(p)).collect::<Vec<_>>(),
        &cfg,
        &cache,
        &CancelToken::new(),
        &|ev| {
            if ev.phase == Phase::FullHash && !done.swap(true, Ordering::SeqCst) {
                // Same size, new content and a new last-write time.
                std::thread::sleep(Duration::from_millis(20));
                std::fs::write(&victim, bytes(51, 2 * MIB)).unwrap();
            }
        },
    );
    let ScanOutcome::Completed(r) = out else {
        panic!("cancelled")
    };
    assert!(done.load(Ordering::SeqCst));
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].files.len(), 2);
    assert!(r.groups[0].files.iter().all(|f| f.path != victim));
    assert!(
        r.skipped
            .iter()
            .any(|s| s.path == victim && s.reason == SkipReason::Changed),
        "{:?}",
        r.skipped
    );
    // Its stale partial hash was invalidated.
    let (id, _, _) = file_info(&victim);
    let m = std::fs::metadata(&victim).unwrap();
    use std::os::windows::fs::MetadataExt;
    let key = HashKey {
        file_ref: FileRef(id),
        size: m.file_size(),
        mtime: strata_core::FileTime(m.last_write_time()),
    };
    assert!(cache.lookup(&volume(), &[key]).unwrap()[0].is_none());
    assert_eq!(cache.len(), 2);
}

#[test]
fn cancel_then_resume_from_cache() {
    let d = TestDir::new("resume");
    let mut paths = Vec::new();
    for i in 0..6 {
        let data = bytes(60 + i, 2 * MIB);
        paths.push(d.file(&format!("p{i}a.bin"), &data));
        paths.push(d.file(&format!("p{i}b.bin"), &data));
    }
    let cands: Vec<Candidate> = paths.iter().map(|p| candidate(p)).collect();
    let cache = MemoryHashCache::new();
    let cfg = ScanConfig {
        concurrency: 1,
        ..cfg()
    };
    let cancel = CancelToken::new();
    let out = find_duplicates(cands.clone(), &cfg, &cache, &cancel, &|ev| {
        if ev.phase == Phase::FullHash && ev.files_done >= 3 {
            cancel.cancel();
        }
    });
    let ScanOutcome::Cancelled(stats) = out else {
        panic!("expected cancellation")
    };
    assert_eq!(stats.groups, 0);
    let saved = cache.full_count();
    assert!((3..12).contains(&saved), "{saved} full hashes cached");

    let r = run(cands, &cfg, &cache);
    assert_eq!(r.groups.len(), 6);
    assert_eq!(r.stats.partial_cached, 12);
    assert_eq!(r.stats.full_cached as usize, saved);
    assert_eq!(r.stats.full_hashed as usize, 12 - saved);
    assert_eq!(cache.full_count(), 12);

    // A third run reads nothing.
    let r = run(paths.iter().map(|p| candidate(p)).collect(), &cfg, &cache);
    assert_eq!(r.groups.len(), 6);
    assert_eq!(r.stats.bytes_read, 0);
}

#[test]
fn placeholders_are_never_read() {
    let d = TestDir::new("placeholder");
    let data = bytes(70, MIB + 1);
    let p = [
        d.file("p1.bin", &data),
        d.file("p2.bin", &data),
        d.file("p3.bin", &data),
    ];
    let mut cands: Vec<Candidate> = p.iter().map(|p| candidate(p)).collect();
    // The index did not know: the attribute appears after the scan. The
    // handle gate must catch it before any read.
    const OFFLINE: u32 = 0x1000;
    set_attributes(&p[2], OFFLINE | 0x20).unwrap();
    let (_, attrs, _) = file_info(&p[2]);
    assert_ne!(attrs & OFFLINE, 0, "offline attribute did not stick");

    // A placeholder the index knows about is never opened at all: this one
    // does not even exist, and is not reported as missing.
    let mut cloud = candidate(&p[0]);
    cloud.path = d.path.join("online-only.bin");
    cloud.file_ref = FileRef(cloud.file_ref.0 + 1);
    cloud.flags = cloud.flags.with_cloud(CloudState::OnlineOnly);
    cands.push(cloud);

    let r = run(cands, &cfg(), &MemoryHashCache::new());
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].files.len(), 2);
    assert_eq!(r.stats.excluded[&Exclusion::CloudPlaceholder], 1);
    assert_eq!(r.skipped.len(), 1, "{:?}", r.skipped);
    assert_eq!(r.skipped[0].path, p[2]);
    assert!(matches!(
        r.skipped[0].reason,
        SkipReason::Placeholder { .. }
    ));
    assert_eq!(r.stats.measured, 3);
}

#[test]
fn wrong_file_reference_is_refused() {
    let d = TestDir::new("idmismatch");
    let data = bytes(80, MIB);
    let a = d.file("a.bin", &data);
    let b = d.file("b.bin", &data);
    let mut cb = candidate(&b);
    cb.file_ref = FileRef(cb.file_ref.0 ^ (1 << 48));
    let r = run(vec![candidate(&a), cb], &cfg(), &MemoryHashCache::new());
    assert!(r.groups.is_empty());
    assert!(matches!(r.skipped[0].reason, SkipReason::IdMismatch { .. }));
}

#[test]
fn progress_reports_every_phase_in_order() {
    let d = TestDir::new("progress");
    let data = bytes(90, 4 * MIB);
    let p = [d.file("a.bin", &data), d.file("b.bin", &data)];
    let events = Mutex::new(Vec::new());
    let cfg = ScanConfig {
        max_bytes_per_sec: Some(40 * MIB as u64),
        read_buffer: 256 * 1024,
        ..cfg()
    };
    let out = find_duplicates(
        p.iter().map(|p| candidate(p)).collect::<Vec<_>>(),
        &cfg,
        &MemoryHashCache::new(),
        &CancelToken::new(),
        &|ev| events.lock().unwrap().push(*ev),
    );
    assert!(matches!(out, ScanOutcome::Completed(_)));
    let ev = events.into_inner().unwrap();
    let mut phases: Vec<Phase> = ev.iter().map(|e| e.phase).collect();
    phases.dedup();
    assert_eq!(
        phases,
        [
            Phase::Grouping,
            Phase::Measuring,
            Phase::PartialHash,
            Phase::FullHash,
            Phase::Finished
        ]
    );
    let full: Vec<&Progress> = ev.iter().filter(|e| e.phase == Phase::FullHash).collect();
    assert_eq!(full[0].bytes_total, 8 * MIB as u64);
    assert!(full.iter().any(|e| e.eta_secs.is_some_and(|s| s > 0.0)));
    assert_eq!(full.last().unwrap().bytes_done, 8 * MIB as u64);
    // Throttled: 8 MiB at 40 MiB/s takes at least ~0.15 s.
    assert!(full.last().unwrap().bytes_per_sec < 60.0 * MIB as f64);
}

#[test]
fn selection_produces_clean_queue_items_that_pass_preflight() {
    let d = TestDir::new("queue");
    let data = bytes(100, MIB + 3);
    let p = [
        d.file("keep.bin", &data),
        d.file(r"dupes\one.bin", &data),
        d.file(r"dupes\two.bin", &data),
    ];
    let r = run(
        p.iter().map(|p| candidate(p)).collect(),
        &cfg(),
        &MemoryHashCache::new(),
    );
    let sel = Selection::all_but_suggested(&r);
    let items = sel
        .to_queue_items(&r, |_| strata_core::Safety::Careful)
        .unwrap();
    assert_eq!(items.len(), 2);
    let keeper = &r.groups[0].files[r.groups[0].keep.index];
    assert!(items.iter().all(|i| i.path != keeper.path));

    let guard = common::guard();
    let plan = strata_clean::flow::plan(&guard, items);
    assert_eq!(plan.items.len(), 2);
    // Copies of user data are "careful": the review screen's tick.
    let acks = strata_clean::flow::Acknowledgements {
        careful: plan.items.iter().map(|i| i.id).collect(),
        ..Default::default()
    };
    let verdicts = strata_clean::flow::preflight(
        &guard,
        &plan,
        &acks,
        &strata_clean::flow::CleanupConfig::default(),
    );
    for v in &verdicts {
        assert!(
            matches!(v.verdict, strata_clean::preflight::Verdict::Ready { .. }),
            "{v:?}"
        );
    }

    // Byte compare before delete: identical now...
    let vcfg = VerifyConfig::default();
    assert!(
        verify_selection(&r, &sel, &vcfg, &CancelToken::new())
            .unwrap()
            .is_empty()
    );
    let hcfg = VerifyConfig {
        mode: VerifyMode::FullHash,
        ..vcfg
    };
    assert!(
        verify_selection(&r, &sel, &hcfg, &CancelToken::new())
            .unwrap()
            .is_empty()
    );
    // ...but a modified copy is caught, and so is the cleaner's pre-flight.
    let changed = items_path(&sel, &r, 0);
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&changed, bytes(101, MIB + 3)).unwrap();
    let f = verify_selection(&r, &sel, &vcfg, &CancelToken::new()).unwrap();
    assert_eq!(f.len(), 1);
    assert!(matches!(
        f[0].problem,
        VerifyProblem::Copy {
            reason: SkipReason::Changed
        }
    ));
    let verdicts = strata_clean::flow::preflight(
        &guard,
        &plan,
        &acks,
        &strata_clean::flow::CleanupConfig::default(),
    );
    assert_eq!(
        verdicts
            .iter()
            .filter(|v| matches!(v.verdict, strata_clean::preflight::Verdict::Blocked { .. }))
            .count(),
        1
    );
}

fn items_path(sel: &Selection, r: &DuplicateReport, n: usize) -> std::path::PathBuf {
    sel.marked_files(r).unwrap()[n].2.path.clone()
}
