//! The store is shared across threads: readers run during long writes, and a
//! reset waits for in-flight calls.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use common::*;
use strata_store::*;

#[test]
fn readers_see_consistent_data_during_writes() {
    let (_d, store, _c) = store_at(t0());
    snap(&store, GIB, synthetic_dirs(1000, 1));
    let stop = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let snaps = store.snapshots(&volume()).unwrap();
                    assert!(!snaps.is_empty());
                    for s in &snaps {
                        assert_eq!(s.stored_dirs, 1000);
                    }
                    let series = store.usage_series(&volume(), None, None).unwrap();
                    assert!(series.len() >= snaps.len());
                    reads += 1;
                }
                reads
            })
        })
        .collect();

    let writer = {
        let store = store.clone();
        thread::spawn(move || {
            for i in 0..10 {
                snap(&store, GIB, synthetic_dirs(1000, i + 2));
                store.save_settings(&Settings::default()).unwrap();
            }
        })
    };
    writer.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        assert!(r.join().unwrap() > 0);
    }
    assert_eq!(store.snapshots(&volume()).unwrap().len(), 11);
}

#[test]
fn concurrent_writers_serialize() {
    let (_d, store, _c) = store_at(t0());
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8u64)
        .map(|t| {
            let store = store.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                for i in 0..20u64 {
                    store
                        .record_activity(&[ActivitySample {
                            at: t0(),
                            image: format!("proc{t}.exe"),
                            dir_hash: i,
                            bytes_written: 1,
                            files_created: 0,
                            files_deleted: 0,
                        }])
                        .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let top = store.top_writers(Timestamp(0), 100).unwrap();
    assert_eq!(top.len(), 8);
    assert!(top.iter().all(|w| w.bytes_written == 20));
}

#[test]
fn reset_while_other_threads_read() {
    let (_d, store, _c) = store_at(t0());
    snap(&store, GIB, vec![dir(r"C:\A", GIB)]);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let store = store.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Either the old or the fresh database; never an error.
                let n = store.snapshots(&volume()).unwrap().len();
                assert!(n <= 1);
            }
        })
    };
    for _ in 0..5 {
        store.reset_history().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    assert!(store.snapshots(&volume()).unwrap().is_empty());
}
