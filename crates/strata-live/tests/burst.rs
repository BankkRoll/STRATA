//! Massive burst: 500,000 files created in one go (an `npm install` or an
//! archive extraction), tailed with the real clock. Work per tick stays
//! bounded, the index lock is held only briefly, progress is reported, and
//! the result equals a fresh build.

mod support;

use std::time::{Duration, Instant};

use strata_index::Index;
use strata_live::{IndexAccess, LiveEvent, LiveStatus, SystemClock, TailerConfig};
use support::{Harness, Model, ROOT_REC, Versions};

/// Measures every lock hold.
struct Timed<'a> {
    index: &'a mut Index,
    longest: Duration,
    holds: u64,
}

impl IndexAccess for Timed<'_> {
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool {
        let t = Instant::now();
        f(self.index);
        self.longest = self.longest.max(t.elapsed());
        self.holds += 1;
        true
    }
}

const FILES: u32 = 500_000;

#[test]
fn half_a_million_changes_stay_bounded_per_tick() {
    let mut m = Model::new(Versions::V2);
    let dirs: Vec<u64> = (0..64)
        .map(|_| m.create_in(ROOT_REC, true, 0, false))
        .collect();
    let cfg = TailerConfig::default();
    let mut h = Harness::new(m, cfg.clone(), None);
    {
        let mut m = h.model.borrow_mut();
        for i in 0..FILES {
            m.create_in(dirs[(i % 64) as usize], false, 0, false);
        }
    }
    let head = h.model.borrow().next_usn;

    let clock = SystemClock;
    let mut events = Vec::new();
    let mut timed = Timed {
        index: &mut h.index,
        longest: Duration::ZERO,
        holds: 0,
    };
    let started = Instant::now();
    while h.tailer.read_position() < head || h.tailer.pending() > 0 {
        h.tailer
            .step(
                &mut h.journal,
                &mut h.records,
                &mut timed,
                &clock,
                &mut |e| events.push(e),
            )
            .expect("step");
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "burst stalled"
        );
    }
    let total = started.elapsed();

    let ticks: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LiveEvent::Tick(t) => Some(t),
            _ => None,
        })
        .collect();
    let progress: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            LiveEvent::Status(LiveStatus::CatchingUp { pending, .. }) => Some(*pending),
            _ => None,
        })
        .collect();
    let slowest = ticks.iter().map(|t| t.elapsed).max().unwrap_or_default();
    let created: usize = ticks.iter().map(|t| t.changes.created.len()).sum();
    println!(
        "burst: {FILES} files, {} ticks, total {total:?}, slowest tick {slowest:?}, \
         longest lock hold {:?} over {} holds, {} progress events",
        ticks.len(),
        timed.longest,
        timed.holds,
        progress.len()
    );

    assert_eq!(created, FILES as usize);
    assert!(ticks.len() > 1, "the burst is spread over several ticks");
    for t in &ticks {
        assert!(
            t.fetched <= cfg.max_refresh_per_tick,
            "{} fetched",
            t.fetched
        );
        // The budget is checked between batches, so one batch may overrun it.
        // NOTE: unoptimised test builds on shared CI runners are several times
        // slower and noisy; the release benchmark is what measures tick time.
        let slack = if cfg!(debug_assertions) { 20 } else { 4 };
        assert!(
            t.elapsed <= cfg.tick_budget * slack,
            "tick took {:?}",
            t.elapsed
        );
    }
    assert!(!progress.is_empty(), "catching-up progress is reported");
    assert_eq!(h.tailer.status(), LiveStatus::Live);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LiveEvent::Status(LiveStatus::Live))),
        "the live state is announced after catching up"
    );
    h.assert_matches_fresh_scan();
}
