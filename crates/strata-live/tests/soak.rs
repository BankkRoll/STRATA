//! Live soak equality: random filesystem churn is journaled the way NTFS
//! journals it, tailed through fake sources backed by the model, and after
//! quiescence the live index must equal a fresh build of the model.
//!
//! The churn covers creates, deletes (recursive), renames and cross-directory
//! moves (pairs split across reads by tiny read buffers), resizes inside open
//! handles (changes with no journal record until the close), hardlinks,
//! alternate streams, reparse points, attribute/time, compression and
//! encryption changes, security-only changes, duplicate late closes, deletes
//! of never-seen references, bursts, V2/V3/V4 records, a tailing start
//! before the scan (replay of already-scanned changes) and app restarts from
//! a cache image mid-stream.
//!
//! `STRATA_PROPTEST_CASES` raises the case count for longer soaks.

mod support;

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use strata_live::TailerConfig;
use support::{Harness, Model, Versions};

#[derive(Debug, Clone)]
enum Op {
    Create {
        dir: u16,
        is_dir: bool,
        size: u32,
        open: bool,
    },
    Delete {
        pick: u16,
    },
    Rename {
        pick: u16,
        dir: u16,
        same_dir: bool,
        open: bool,
    },
    Write {
        pick: u16,
        size: u32,
        open: bool,
        range: bool,
    },
    Close {
        pick: u16,
    },
    LinkAdd {
        pick: u16,
        dir: u16,
    },
    LinkRemove {
        pick: u16,
    },
    Ads {
        pick: u16,
        stream: u8,
        size: u32,
    },
    Reparse {
        pick: u16,
    },
    Basic {
        pick: u16,
    },
    Compress {
        pick: u16,
    },
    Encrypt {
        pick: u16,
    },
    Security {
        pick: u16,
    },
    DupClose {
        pick: u16,
    },
    Ghost {
        pick: u16,
    },
    Burst {
        dir: u16,
        n: u8,
        delete_some: bool,
    },
    Tick {
        ms: u16,
        max_bytes: u16,
    },
    Settle,
    Restart,
}

fn op() -> impl Strategy<Value = Op> {
    let k = any::<u16>;
    let size = || prop_oneof![Just(0u32), 1u32..5000, 5000u32..10_000_000];
    prop_oneof![
        8 => (k(), any::<bool>(), size(), any::<bool>())
            .prop_map(|(dir, is_dir, size, open)| Op::Create { dir, is_dir, size, open }),
        3 => k().prop_map(|pick| Op::Delete { pick }),
        5 => (k(), k(), any::<bool>(), any::<bool>())
            .prop_map(|(pick, dir, same_dir, open)| Op::Rename { pick, dir, same_dir, open }),
        6 => (k(), size(), any::<bool>(), any::<bool>())
            .prop_map(|(pick, size, open, range)| Op::Write { pick, size, open, range }),
        2 => k().prop_map(|pick| Op::Close { pick }),
        2 => (k(), k()).prop_map(|(pick, dir)| Op::LinkAdd { pick, dir }),
        1 => k().prop_map(|pick| Op::LinkRemove { pick }),
        2 => (k(), any::<u8>(), size()).prop_map(|(pick, stream, size)| Op::Ads { pick, stream, size }),
        1 => k().prop_map(|pick| Op::Reparse { pick }),
        2 => k().prop_map(|pick| Op::Basic { pick }),
        1 => k().prop_map(|pick| Op::Compress { pick }),
        1 => k().prop_map(|pick| Op::Encrypt { pick }),
        1 => k().prop_map(|pick| Op::Security { pick }),
        1 => k().prop_map(|pick| Op::DupClose { pick }),
        1 => k().prop_map(|pick| Op::Ghost { pick }),
        1 => (k(), 1u8..60, any::<bool>())
            .prop_map(|(dir, n, delete_some)| Op::Burst { dir, n, delete_some }),
        8 => (0u16..400, prop_oneof![Just(64u16), 64u16..600, Just(u16::MAX)])
            .prop_map(|(ms, max_bytes)| Op::Tick { ms, max_bytes }),
        1 => Just(Op::Settle),
        1 => Just(Op::Restart),
    ]
}

#[derive(Debug, Clone)]
struct Case {
    versions: Versions,
    ranges: bool,
    /// Ops applied before the scan; tailing starts before or after them.
    setup: Vec<Op>,
    replay_setup: bool,
    ops: Vec<Op>,
    cfg_small: bool,
}

fn case(max_ops: usize) -> impl Strategy<Value = Case> {
    (
        prop_oneof![
            Just(Versions::V2),
            Just(Versions::V3),
            Just(Versions::Mixed)
        ],
        any::<bool>(),
        prop::collection::vec(op(), 0..40),
        any::<bool>(),
        prop::collection::vec(op(), 1..max_ops),
        any::<bool>(),
    )
        .prop_map(
            |(versions, ranges, setup, replay_setup, ops, cfg_small)| Case {
                versions,
                ranges,
                setup,
                replay_setup,
                ops,
                cfg_small,
            },
        )
}

fn apply(h: &mut Harness, op: &Op) {
    let Some(m) = (match op {
        Op::Tick { ms, max_bytes } => {
            h.journal.max_bytes.set(usize::from(*max_bytes));
            h.tick(u64::from(*ms)).expect("tick");
            None
        }
        Op::Settle => {
            h.settle().expect("settle");
            h.assert_matches_fresh_scan();
            None
        }
        Op::Restart => {
            h.restart();
            None
        }
        _ => Some(h.model.clone()),
    }) else {
        return;
    };
    mutate(&mut m.borrow_mut(), op);
}

fn mutate(m: &mut Model, op: &Op) {
    match *op {
        Op::Create {
            dir,
            is_dir,
            size,
            open,
        } => {
            m.create(dir, is_dir, size, open);
        }
        Op::Delete { pick } => m.delete(pick),
        Op::Rename {
            pick,
            dir,
            same_dir,
            open,
        } => m.rename(pick, dir, same_dir, open),
        Op::Write {
            pick,
            size,
            open,
            range,
        } => m.write(pick, size, open, range),
        Op::Close { pick } => m.close_pick(pick),
        Op::LinkAdd { pick, dir } => m.link_add(pick, dir),
        Op::LinkRemove { pick } => m.link_remove(pick),
        Op::Ads { pick, stream, size } => m.ads(pick, stream, size),
        Op::Reparse { pick } => m.reparse(pick),
        Op::Basic { pick } => m.basic(pick),
        Op::Compress { pick } => m.compress(pick),
        Op::Encrypt { pick } => m.encrypt(pick),
        Op::Security { pick } => m.security(pick),
        Op::DupClose { pick } => m.dup_close(pick),
        Op::Ghost { pick } => m.ghost_delete(pick),
        Op::Burst {
            dir,
            n,
            delete_some,
        } => {
            let mut made = Vec::new();
            for i in 0..n {
                made.push(m.create(dir, i % 7 == 0, u32::from(i) * 977, false));
            }
            if delete_some {
                for (i, _) in made.iter().enumerate().filter(|(i, _)| i % 3 == 0) {
                    m.delete(i as u16);
                }
            }
        }
        Op::Tick { .. } | Op::Settle | Op::Restart => {}
    }
}

fn run(c: &Case) -> Harness {
    let mut model = Model::new(c.versions);
    model.ranges = c.ranges;
    for op in &c.setup {
        mutate(&mut model, op);
    }
    model.close_all();
    let start = c.replay_setup.then_some(0);
    let cfg = if c.cfg_small {
        // Tiny batches and per-tick caps force backlogs and carry-over.
        TailerConfig {
            fetch_batch: 3,
            apply_batch: 2,
            max_refresh_per_tick: 7,
            max_pending: 20,
            ..TailerConfig::default()
        }
    } else {
        TailerConfig::default()
    };
    let mut h = Harness::new(model, cfg, start);
    for op in &c.ops {
        apply(&mut h, op);
    }
    h.settle().expect("final settle");
    h.assert_matches_fresh_scan();
    h
}

fn cases() -> u32 {
    std::env::var("STRATA_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(96)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn live_index_equals_fresh_scan(c in case(160)) {
        run(&c);
    }
}

/// One long deterministic soak: thousands of operations, many settle
/// points, restarts, and every record version.
#[test]
fn long_soak() {
    let mut runner = TestRunner::deterministic();
    let mut stats = (0u64, 0u64, 0u64, 0u64);
    for versions in [Versions::V2, Versions::V3, Versions::Mixed] {
        let ops = prop::collection::vec(op(), 3000)
            .new_tree(&mut runner)
            .expect("ops")
            .current();
        let c = Case {
            versions,
            ranges: true,
            setup: Vec::new(),
            replay_setup: false,
            ops,
            cfg_small: versions == Versions::Mixed,
        };
        let h = run(&c);
        let s = h.total_stats();
        stats.0 += s.records;
        stats.1 += s.paired_renames;
        stats.2 += s.ignored;
        stats.3 += h.model.borrow().fetched_refs;
    }
    let (records, paired, ignored, fetched) = stats;
    assert!(records > 5000, "records {records}");
    assert!(paired > 50, "paired renames {paired}");
    assert!(ignored > 0, "ignored {ignored}");
    // Coalescing: far fewer fetches than records.
    assert!(fetched < records, "fetched {fetched} of {records} records");
}
