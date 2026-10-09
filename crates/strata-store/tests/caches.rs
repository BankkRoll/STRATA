//! ETW rollups, last-writer rows, the duplicate hash cache and license
//! storage.

mod common;

use common::*;
use strata_core::{FileRef, FileTime};
use strata_store::*;

fn sample(at: Timestamp, image: &str, dir: &str, bytes: u64) -> ActivitySample {
    ActivitySample {
        at,
        image: image.into(),
        dir_hash: path_hash(dir),
        bytes_written: bytes,
        files_created: 1,
        files_deleted: 0,
    }
}

const CHROME: &str = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
const CODE: &str = r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe";

#[test]
fn hourly_rollups_sum_within_the_hour() {
    let (_d, store, _c) = store_at(t0());
    let h = t0().hour_start();
    store
        .record_activity(&[
            sample(Timestamp(h.0 + 10), CHROME, r"C:\Cache", 100),
            sample(Timestamp(h.0 + 3599), CHROME, r"C:\Cache", 50),
            sample(Timestamp(h.0 + 3600), CHROME, r"C:\Cache", 7),
            sample(Timestamp(h.0 + 20), CODE, r"C:\Dev", 500),
        ])
        .unwrap();
    store
        .record_activity(&[sample(Timestamp(h.0 + 30), CHROME, r"C:\Cache", 1)])
        .unwrap();

    let top = store.top_writers(h, 10).unwrap();
    assert_eq!(top[0].image, CODE);
    assert_eq!(top[1].image, CHROME);
    assert_eq!(top[1].bytes_written, 158);
    assert_eq!(top[1].files_created, 4);

    let last_hour = store.top_writers(Timestamp(h.0 + 3600), 10).unwrap();
    assert_eq!(last_hour.len(), 1);
    assert_eq!(last_hour[0].bytes_written, 7);

    let cache = store.dir_writers(path_hash(r"c:\cache\"), h, 10).unwrap();
    assert_eq!(cache.len(), 1);
    assert_eq!(cache[0].image, CHROME);
    assert_eq!(store.top_writers(h, 1).unwrap().len(), 1);
}

#[test]
fn activity_prune_and_clear() {
    let (_d, store, clock) = store_at(t0());
    store
        .record_activity(&[
            sample(t0().minus_days(40), CHROME, r"C:\Old", 1),
            sample(t0(), CODE, r"C:\New", 1),
        ])
        .unwrap();
    store
        .set_last_writers(&[
            LastWrite {
                path_hash: path_hash(r"C:\Old\f"),
                image: CHROME.into(),
                pid: Some(4),
                at: t0().minus_days(40),
            },
            LastWrite {
                path_hash: path_hash(r"C:\New\f"),
                image: CODE.into(),
                pid: None,
                at: t0(),
            },
        ])
        .unwrap();
    let report = store.prune_activity(30).unwrap();
    assert_eq!(report.hourly_rows, 1);
    assert_eq!(report.last_writer_rows, 1);
    let all = store.top_writers(Timestamp(0), 10).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].image, CODE);
    assert!(store.last_writer(path_hash(r"C:\Old\f")).unwrap().is_none());

    clock.advance_secs(1);
    store.clear_activity().unwrap();
    assert!(store.top_writers(Timestamp(0), 10).unwrap().is_empty());
    assert!(store.last_writer(path_hash(r"C:\New\f")).unwrap().is_none());
}

#[test]
fn last_writer_keeps_newest() {
    let (_d, store, _c) = store_at(t0());
    let h = path_hash(r"C:\file.bin");
    let w = |image: &str, at: i64| LastWrite {
        path_hash: h,
        image: image.into(),
        pid: Some(42),
        at: Timestamp(at),
    };
    store.set_last_writers(&[w(CHROME, 200)]).unwrap();
    store.set_last_writers(&[w(CODE, 100)]).unwrap();
    assert_eq!(store.last_writer(h).unwrap().unwrap().image, CHROME);
    store.set_last_writers(&[w(CODE, 300)]).unwrap();
    let got = store.last_writer(h).unwrap().unwrap();
    assert_eq!(
        (got.image.as_str(), got.at, got.pid),
        (CODE, Timestamp(300), Some(42))
    );
}

fn key(r: u64, size: u64, mtime: u64) -> HashKey {
    HashKey {
        file_ref: FileRef(r),
        size,
        mtime: FileTime(mtime),
    }
}

#[test]
fn hash_cache_hits_only_on_matching_identity() {
    let (_d, store, _c) = store_at(t0());
    let v = volume();
    let full = [7u8; 32];
    store
        .upsert_hashes(
            &v,
            &[
                CachedHash {
                    key: key(1, 100, 5),
                    partial: 11,
                    full: Some(full),
                },
                CachedHash {
                    key: key(u64::MAX, u64::MAX >> 1, u64::MAX),
                    partial: u64::MAX,
                    full: None,
                },
            ],
        )
        .unwrap();
    let got = store
        .lookup_hashes(
            &v,
            &[
                key(1, 100, 5),
                key(1, 101, 5),
                key(1, 100, 6),
                key(2, 100, 5),
                key(u64::MAX, u64::MAX >> 1, u64::MAX),
            ],
        )
        .unwrap();
    assert_eq!(got[0].unwrap().full, Some(full));
    assert!(got[1].is_none() && got[2].is_none() && got[3].is_none());
    assert_eq!(got[4].unwrap().partial, u64::MAX);
    let other = store
        .lookup_hashes(&other_volume(), &[key(1, 100, 5)])
        .unwrap();
    assert!(other[0].is_none(), "volumes are separate");
}

#[test]
fn partial_only_upsert_keeps_full_for_same_content() {
    let (_d, store, _c) = store_at(t0());
    let v = volume();
    let with_full = CachedHash {
        key: key(1, 100, 5),
        partial: 11,
        full: Some([1; 32]),
    };
    store.upsert_hashes(&v, &[with_full]).unwrap();
    store
        .upsert_hashes(
            &v,
            &[CachedHash {
                full: None,
                ..with_full
            }],
        )
        .unwrap();
    assert_eq!(
        store.lookup_hashes(&v, &[with_full.key]).unwrap()[0]
            .unwrap()
            .full,
        Some([1; 32])
    );
    // Changed content drops the stale full hash.
    let changed = CachedHash {
        key: key(1, 200, 9),
        partial: 12,
        full: None,
    };
    store.upsert_hashes(&v, &[changed]).unwrap();
    assert_eq!(
        store.lookup_hashes(&v, &[changed.key]).unwrap()[0],
        Some(changed)
    );
}

#[test]
fn hash_invalidation_and_clear() {
    let (_d, store, _c) = store_at(t0());
    let v = volume();
    let entries: Vec<_> = (0..1000)
        .map(|i| CachedHash {
            key: key(i, i * 10, i),
            partial: i,
            full: None,
        })
        .collect();
    store.upsert_hashes(&v, &entries).unwrap();
    let removed = store
        .invalidate_hashes(&v, &[FileRef(1), FileRef(2), FileRef(5000)])
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(
        store
            .invalidate_hashes(&other_volume(), &[FileRef(3)])
            .unwrap(),
        0
    );
    let keys: Vec<_> = entries.iter().map(|e| e.key).collect();
    let hits = store.lookup_hashes(&v, &keys).unwrap();
    assert_eq!(hits.iter().filter(|h| h.is_some()).count(), 998);
    store.clear_hash_cache().unwrap();
    assert!(
        store
            .lookup_hashes(&v, &keys)
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn license_save_replace_clear() {
    let (_d, store, clock) = store_at(t0());
    assert!(store.load_license().unwrap().is_none());
    store.save_license(b"first", &[1; 64]).unwrap();
    clock.advance_secs(100);
    let saved = store.save_license(b"second", &[2; 64]).unwrap();
    let loaded = store.load_license().unwrap().unwrap();
    assert_eq!(loaded, saved);
    assert_eq!(loaded.activated_at, Timestamp(t0().0 + 100));
    store.clear_license().unwrap();
    assert!(store.load_license().unwrap().is_none());
}

#[test]
fn history_reset_keeps_license() {
    let (_d, store, _c) = store_at(t0());
    store.save_license(b"payload", b"sig").unwrap();
    store.reset_history().unwrap();
    assert!(store.load_license().unwrap().is_some());
}
