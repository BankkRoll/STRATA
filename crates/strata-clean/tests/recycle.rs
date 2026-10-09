//! Recycle Bin delete and restore, round-trip on temp files.
//!
//! Every Recycle Bin entry a test creates is either restored or purged
//! (exactly that `$R`/`$I` pair) when the test ends.

mod common;

use std::path::{Path, PathBuf};

use common::{Holder, TestDir, guard, swap_for_junction};
use strata_clean::recycle::{
    RecycleItem, RestoreError, RestoreTicket, find_in_recycle_bin, recycle, restore,
};
use strata_clean::{CancelToken, Change, CleanError, Expected};

/// Purges the bin entries of tickets that were not restored.
struct BinCleanup(Vec<RestoreTicket>);

impl BinCleanup {
    fn track(&mut self, t: &RestoreTicket) {
        self.0.push(t.clone());
    }
}

impl Drop for BinCleanup {
    fn drop(&mut self) {
        for t in &self.0 {
            let r = Path::new(&t.recycled_path);
            assert!(
                t.recycled_path.to_uppercase().contains(r"\$RECYCLE.BIN\"),
                "refusing to purge outside the bin: {}",
                t.recycled_path
            );
            if r.is_dir() {
                common::unlink_reparse_points(r);
                let _ = std::fs::remove_dir_all(r);
            } else {
                let _ = std::fs::remove_file(r);
            }
            let _ = std::fs::remove_file(&t.info_path);
        }
    }
}

fn item(p: &Path) -> RecycleItem {
    RecycleItem {
        path: p.to_path_buf(),
        expected: Expected::from_facts(&guard().check_path(p).unwrap().facts),
    }
}

fn recycle_one(p: &Path, bin: &mut BinCleanup) -> RestoreTicket {
    let mut r = recycle(guard(), &[item(p)], &CancelToken::new());
    let t = r
        .remove(0)
        .unwrap_or_else(|e| panic!("{}: {e:?}", p.display()));
    bin.track(&t);
    t
}

#[test]
fn file_round_trip() {
    let mut bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-file");
    let f = t.file("hello.txt", b"hello recycle bin");
    let ticket = recycle_one(&f, &mut bin);
    assert!(!f.exists());
    assert!(Path::new(&ticket.recycled_path).exists());
    assert!(Path::new(&ticket.info_path).exists());
    assert_eq!(ticket.size, 17);
    assert_eq!(
        ticket.original_path.to_lowercase(),
        f.display().to_string().to_lowercase()
    );

    // The undo log stores the blob; restore from it.
    let blob = ticket.to_blob();
    let back = restore(&RestoreTicket::from_blob(&blob).unwrap()).unwrap();
    assert_eq!(
        back.to_string_lossy().to_lowercase(),
        f.display().to_string().to_lowercase()
    );
    assert_eq!(std::fs::read(&f).unwrap(), b"hello recycle bin");
    assert!(!Path::new(&ticket.info_path).exists());
    assert_eq!(restore(&ticket), Err(RestoreError::NotInRecycleBin));
}

#[test]
fn directory_round_trip_recreates_missing_parent() {
    let mut bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-dir");
    let d = t.dir(r"parent\folder");
    t.file(r"parent\folder\a.txt", b"a");
    t.file(r"parent\folder\sub\b.txt", b"b");
    let mut it = item(&d);
    it.expected.size = 2;
    let ticket = recycle(guard(), &[it], &CancelToken::new())
        .remove(0)
        .unwrap();
    bin.track(&ticket);
    assert!(!d.exists());
    std::fs::remove_dir(t.path.join("parent")).unwrap();
    restore(&ticket).unwrap();
    assert_eq!(std::fs::read(d.join(r"sub\b.txt")).unwrap(), b"b");
}

#[test]
fn batch_with_a_locked_item_recycles_the_rest() {
    let mut bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-batch");
    let a = t.file("a.txt", b"a");
    let b = t.file("b.txt", b"b");
    let c = t.file("c.txt", b"c");
    let items = vec![item(&a), item(&b), item(&c)];
    let holder = Holder::spawn(&b);
    let results = recycle(guard(), &items, &CancelToken::new());
    drop(holder);
    for r in results.iter().flatten() {
        bin.track(r);
    }
    assert!(results[0].is_ok(), "{:?}", results[0]);
    assert!(
        matches!(
            results[1],
            Err(CleanError::Locked { .. }) | Err(CleanError::Os { .. })
        ),
        "{:?}",
        results[1]
    );
    assert!(results[2].is_ok(), "{:?}", results[2]);
    assert!(b.exists());
    assert!(!a.exists() && !c.exists());
    for r in results.into_iter().flatten() {
        restore(&r).unwrap();
    }
    assert!(a.exists() && c.exists());
}

#[test]
fn restore_never_overwrites() {
    let mut bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-conflict");
    let f = t.file("same.txt", b"old");
    let ticket = recycle_one(&f, &mut bin);
    std::fs::write(&f, b"new").unwrap();
    assert!(matches!(
        restore(&ticket),
        Err(RestoreError::DestinationExists { .. })
    ));
    assert_eq!(std::fs::read(&f).unwrap(), b"new");
}

#[test]
fn tampered_ticket_is_refused() {
    let mut bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-tamper");
    let f = t.file("x.txt", b"x");
    let ticket = recycle_one(&f, &mut bin);
    let mut bad = ticket.clone();
    bad.original_path = r"C:\Windows\System32\x.txt".into();
    assert_eq!(restore(&bad), Err(RestoreError::Mismatch));
    assert!(!Path::new(r"C:\Windows\System32\x.txt").exists());
    restore(&ticket).unwrap();
}

#[test]
fn find_recovers_tickets() {
    let mut bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-find");
    let f = t.file("findme.txt", b"f");
    let ticket = recycle_one(&f, &mut bin);
    let found = find_in_recycle_bin(&f).unwrap();
    assert!(
        found
            .iter()
            .any(|x| x.recycled_path.eq_ignore_ascii_case(&ticket.recycled_path))
    );
    restore(&found[0]).unwrap();
    assert!(f.exists());
}

#[test]
fn long_paths_recycle_and_restore() {
    let mut bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-long");
    let mut dir = t.path.clone();
    for _ in 0..28 {
        dir.push("abcdefghij");
    }
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("long.txt");
    std::fs::write(&f, b"long").unwrap();
    assert!(f.as_os_str().len() > 300);
    // The Shell would delete this permanently; it must be reported up front.
    let r = recycle(guard(), &[item(&f)], &CancelToken::new()).remove(0);
    assert!(
        matches!(
            r,
            Err(CleanError::RecycleBinUnavailable {
                reason: strata_clean::volume::RecycleUnavailable::PathTooLong,
                ..
            })
        ),
        "{r:?}"
    );
    assert_eq!(std::fs::read(&f).unwrap(), b"long");

    // Just under the limit still recycles and restores.
    let mut short = t.path.clone();
    while short.as_os_str().len() < 230 {
        short.push("abcdefghij");
    }
    std::fs::create_dir_all(&short).unwrap();
    let g = short.join("ok.txt");
    std::fs::write(&g, b"ok").unwrap();
    let ticket = recycle_one(&g, &mut bin);
    assert!(!g.exists());
    restore(&ticket).unwrap();
    assert_eq!(std::fs::read(&g).unwrap(), b"ok");
    bin.0.clear();
}

#[test]
fn race_swapped_dir_is_not_recycled() {
    let _bin = BinCleanup(Vec::new());
    let t = TestDir::new("rb-race");
    let victim = t.dir("victim");
    let other = t.dir("other");
    std::fs::write(other.join("sentinel.txt"), b"s").unwrap();
    let it = item(&victim);
    swap_for_junction(&victim, &other);
    let r = recycle(guard(), &[it], &CancelToken::new()).remove(0);
    assert!(
        matches!(
            r,
            Err(CleanError::Changed {
                change: Change::Identity { .. },
                ..
            })
        ),
        "{r:?}"
    );
    assert!(other.join("sentinel.txt").exists());
    assert!(victim.exists());
}

#[test]
fn protected_and_changed_items_never_reach_the_shell() {
    let t = TestDir::new("rb-refuse");
    let f = t.file("f.txt", b"1");
    let mut changed = item(&f);
    changed.expected.size = 999;
    let windows = RecycleItem {
        path: PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts"),
        expected: changed.expected,
    };
    let r = recycle(guard(), &[changed, windows], &CancelToken::new());
    assert!(
        matches!(r[0], Err(CleanError::Changed { .. })),
        "{:?}",
        r[0]
    );
    assert!(
        matches!(r[1], Err(CleanError::Refused { .. })),
        "{:?}",
        r[1]
    );
    assert!(f.exists());
}

#[test]
fn cancelled_batch_recycles_nothing() {
    let t = TestDir::new("rb-cancel");
    let f = t.file("f.txt", b"1");
    let c = CancelToken::new();
    c.cancel();
    let r = recycle(guard(), &[item(&f)], &c);
    assert!(matches!(r[0], Err(CleanError::Cancelled { .. })));
    assert!(f.exists());
}
