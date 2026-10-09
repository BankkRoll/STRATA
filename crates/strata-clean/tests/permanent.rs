//! Permanent delete by handle, including TOCTOU race tests (SPEC §22).

mod common;

use std::path::Path;

use common::{TestDir, guard, junction, swap_for_junction};
use strata_clean::permanent::delete_permanently;
use strata_clean::{CancelToken, Change, CleanError, Expected};

fn expect(p: &Path) -> Expected {
    Expected::from_facts(&guard().check_path(p).unwrap().facts)
}

#[test]
fn deletes_a_file() {
    let t = TestDir::new("perm-file");
    let f = t.file("a.bin", &[1u8; 100]);
    let stats = delete_permanently(guard(), &f, &expect(&f), &CancelToken::new()).unwrap();
    assert_eq!((stats.files, stats.bytes), (1, 100));
    assert!(!f.exists());
}

#[test]
fn deletes_a_tree_and_unlinks_junctions_without_following() {
    let t = TestDir::new("perm-tree");
    let outside = t.dir("outside");
    let sentinel = outside.join("sentinel.txt");
    std::fs::write(&sentinel, b"must survive").unwrap();

    let root = t.dir("victim");
    t.file(r"victim\a.txt", b"a");
    t.file(r"victim\sub\b.txt", b"bb");
    t.file(r"victim\sub\deeper\c.txt", b"ccc");
    let ro = t.file(r"victim\sub\readonly.txt", b"r");
    let mut perms = std::fs::metadata(&ro).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&ro, perms).unwrap();
    junction(&root.join(r"sub\link-out"), &outside);
    junction(&root.join("link-windows"), Path::new(r"C:\Windows"));

    let stats = delete_permanently(guard(), &root, &expect(&root), &CancelToken::new()).unwrap();
    assert!(!root.exists());
    assert_eq!(stats.files, 4);
    assert_eq!(stats.links, 2);
    assert_eq!(stats.dirs, 3);
    assert_eq!(stats.bytes, 1 + 2 + 3 + 1);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"must survive");
    assert!(Path::new(r"C:\Windows\System32").exists());
}

#[test]
fn junction_item_is_unlinked_not_its_target() {
    let t = TestDir::new("perm-link");
    let target = t.dir("target");
    std::fs::write(target.join("keep.txt"), b"k").unwrap();
    let link = t.path.join("link");
    junction(&link, &target);
    let stats = delete_permanently(guard(), &link, &expect(&link), &CancelToken::new()).unwrap();
    assert_eq!(stats.links, 1);
    assert!(!link.exists());
    assert!(target.join("keep.txt").exists());
}

#[test]
fn deep_and_long_paths() {
    let t = TestDir::new("perm-deep");
    let mut p = t.path.join("deep");
    for i in 0..300 {
        p.push(format!("d{i:03}"));
    }
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("leaf.txt"), b"x").unwrap();
    assert!(p.as_os_str().len() > 1500);
    let root = t.path.join("deep");
    let stats = delete_permanently(guard(), &root, &expect(&root), &CancelToken::new()).unwrap();
    assert_eq!((stats.files, stats.dirs), (1, 301));
    assert!(!root.exists());

    // A long-path file deleted directly.
    let mut q = t.path.join("long");
    for _ in 0..30 {
        q.push("0123456789");
    }
    std::fs::create_dir_all(&q).unwrap();
    let f = q.join("file.txt");
    std::fs::write(&f, b"y").unwrap();
    delete_permanently(guard(), &f, &expect(&f), &CancelToken::new()).unwrap();
    assert!(!f.exists());
}

#[test]
fn race_dir_swapped_for_junction_is_refused() {
    let t = TestDir::new("race-swap");
    let victim = t.dir("victim");
    t.file(r"victim\x.txt", b"x");
    let other = t.dir("other");
    let sentinel = other.join("sentinel.txt");
    std::fs::write(&sentinel, b"sentinel").unwrap();

    // Pre-flight saw the real directory.
    let expected = expect(&victim);
    // Attacker swaps it for a junction to another directory.
    swap_for_junction(&victim, &other);

    let err = delete_permanently(guard(), &victim, &expected, &CancelToken::new()).unwrap_err();
    assert!(
        matches!(
            err,
            CleanError::Changed {
                change: Change::Identity { .. },
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"sentinel");
    assert!(victim.exists());
}

#[test]
fn race_dir_swapped_for_junction_into_windows_is_refused() {
    let t = TestDir::new("race-swap-win");
    let victim = t.dir("victim");
    let expected = expect(&victim);
    swap_for_junction(&victim, Path::new(r"C:\Windows\System32"));
    let err = delete_permanently(guard(), &victim, &expected, &CancelToken::new()).unwrap_err();
    assert!(matches!(err, CleanError::Refused { .. }), "{err:?}");
    // And through the junction: the parent chain resolves into Windows.
    let child = victim.join("drivers");
    let err = delete_permanently(guard(), &child, &expected, &CancelToken::new()).unwrap_err();
    assert!(matches!(err, CleanError::Refused { .. }), "{err:?}");
}

#[test]
fn race_file_replaced_or_modified_is_refused() {
    let t = TestDir::new("race-file");
    let f = t.file("f.txt", b"original");
    let expected = expect(&f);
    std::fs::write(&f, b"changed!").unwrap();
    let err = delete_permanently(guard(), &f, &expected, &CancelToken::new()).unwrap_err();
    assert!(matches!(err, CleanError::Changed { .. }), "{err:?}");

    let g = t.file("g.txt", b"same");
    let expected = expect(&g);
    std::fs::remove_file(&g).unwrap();
    std::fs::write(&g, b"same").unwrap();
    let err = delete_permanently(guard(), &g, &expected, &CancelToken::new()).unwrap_err();
    assert!(
        matches!(
            err,
            CleanError::Changed {
                change: Change::Identity { .. } | Change::Modified { .. },
                ..
            }
        ),
        "{err:?}"
    );
    assert!(g.exists());
}

#[test]
fn race_file_locked_after_preflight_is_a_typed_failure() {
    let t = TestDir::new("race-lock");
    let f = t.file("locked.txt", b"data");
    let expected = expect(&f);
    let _holder = common::Holder::spawn(&f);
    let err = delete_permanently(guard(), &f, &expected, &CancelToken::new()).unwrap_err();
    assert!(matches!(err, CleanError::Locked { .. }), "{err:?}");
    assert!(err.is_retryable());
    drop(_holder);
    assert!(f.exists());
}

#[test]
fn locked_child_stops_the_walk_with_partial() {
    let t = TestDir::new("partial");
    let root = t.dir("root");
    let locked = t.file(r"root\z-locked.txt", b"l");
    let expected = expect(&root);
    let holder = common::Holder::spawn(&locked);
    let err = delete_permanently(guard(), &root, &expected, &CancelToken::new()).unwrap_err();
    match err {
        CleanError::Locked { .. } | CleanError::Partial { .. } => {}
        other => panic!("{other:?}"),
    }
    drop(holder);
    assert!(locked.exists());
}

#[test]
fn cancelled_before_start_deletes_nothing() {
    let t = TestDir::new("cancel");
    let f = t.file("c.txt", b"c");
    let c = CancelToken::new();
    c.cancel();
    let err = delete_permanently(guard(), &f, &expect(&f), &c).unwrap_err();
    assert!(matches!(err, CleanError::Cancelled { .. }));
    assert!(f.exists());
}

#[test]
fn protected_paths_are_refused_before_opening() {
    let e = Expected {
        file_ref: strata_core::FileRef(5),
        is_dir: true,
        size: 0,
        modified: strata_core::FileTime(0),
    };
    for p in [
        r"C:\Windows",
        r"C:\Users",
        r"C:\",
        r"C:\Windows\Temp",
        r"C:\PROGRA~1",
    ] {
        let err = delete_permanently(guard(), Path::new(p), &e, &CancelToken::new()).unwrap_err();
        assert!(
            matches!(
                err,
                CleanError::Refused { .. } | CleanError::NotFound { .. }
            ),
            "{p}: {err:?}"
        );
    }
}
