//! The helper's by-id delete, run unelevated on our own temp files.

mod common;

use std::path::Path;

use common::{TestDir, guard, junction};
use strata_clean::privileged::{
    DelayedDeleteRequest, PrivilegedDeleteRequest, verify_and_delete_by_id,
};
use strata_clean::{CancelToken, CleanError};
use strata_core::FileRef;

fn request(p: &Path) -> PrivilegedDeleteRequest {
    let c = guard().check_path(p).unwrap();
    let vol = strata_clean::volume::volume_info(&c.resolved).unwrap();
    PrivilegedDeleteRequest {
        volume: vol.guid.unwrap(),
        file_ref: FileRef(c.facts.identity.file_index),
        expected_path: p.display().to_string(),
        expected_size: c.facts.size,
        expected_mtime: c.facts.modified,
        is_dir: c.facts.is_dir(),
    }
}

#[test]
fn deletes_by_id_after_verifying_everything() {
    let t = TestDir::new("priv-ok");
    let f = t.file("a.txt", b"abc");
    let r = request(&f);
    let stats = verify_and_delete_by_id(guard(), &r, &CancelToken::new()).unwrap();
    assert_eq!(stats.files, 1);
    assert!(!f.exists());
}

#[test]
fn mount_point_volume_form_also_works() {
    let t = TestDir::new("priv-mount");
    let f = t.file("a.txt", b"abc");
    let mut r = request(&f);
    r.volume = format!("{}:\\", &f.display().to_string()[..1]);
    verify_and_delete_by_id(guard(), &r, &CancelToken::new()).unwrap();
    assert!(!f.exists());
}

#[test]
fn deletes_a_directory_by_id_without_following_links() {
    let t = TestDir::new("priv-dir");
    let outside = t.dir("outside");
    std::fs::write(outside.join("keep.txt"), b"k").unwrap();
    let d = t.dir("victim");
    t.file(r"victim\x\y.txt", b"y");
    junction(&d.join("out"), &outside);
    let r = request(&d);
    let stats = verify_and_delete_by_id(guard(), &r, &CancelToken::new()).unwrap();
    assert_eq!((stats.files, stats.links), (1, 1));
    assert!(!d.exists());
    assert!(outside.join("keep.txt").exists());
}

#[test]
fn renamed_file_is_refused() {
    let t = TestDir::new("priv-rename");
    let f = t.file("a.txt", b"abc");
    let r = request(&f);
    let moved = t.path.join("b.txt");
    std::fs::rename(&f, &moved).unwrap();
    let e = verify_and_delete_by_id(guard(), &r, &CancelToken::new()).unwrap_err();
    assert!(matches!(e, CleanError::Changed { .. }), "{e:?}");
    assert!(moved.exists());
}

#[test]
fn modified_file_is_refused() {
    let t = TestDir::new("priv-mod");
    let f = t.file("a.txt", b"abc");
    let r = request(&f);
    std::fs::write(&f, b"abcd").unwrap();
    let e = verify_and_delete_by_id(guard(), &r, &CancelToken::new()).unwrap_err();
    assert!(matches!(e, CleanError::Changed { .. }), "{e:?}");
    assert!(f.exists());
}

#[test]
fn path_swap_cannot_redirect_the_delete() {
    // The request names file A by id. The attacker swaps A's parent for a
    // junction so the *path* now names file B. By-id open still finds A,
    // whose real path no longer matches, so nothing is deleted.
    let t = TestDir::new("priv-swap");
    let real = t.dir("real");
    let a = t.file(r"real\a.txt", b"A");
    let r = request(&a);
    let elsewhere = t.dir("elsewhere");
    std::fs::write(elsewhere.join("a.txt"), b"B").unwrap();
    std::fs::rename(&real, t.path.join("moved")).unwrap();
    junction(&real, &elsewhere);
    let e = verify_and_delete_by_id(guard(), &r, &CancelToken::new()).unwrap_err();
    assert!(matches!(e, CleanError::Changed { .. }), "{e:?}");
    assert_eq!(std::fs::read(elsewhere.join("a.txt")).unwrap(), b"B");
    assert_eq!(std::fs::read(t.path.join(r"moved\a.txt")).unwrap(), b"A");
}

#[test]
fn wrong_volume_is_refused() {
    let t = TestDir::new("priv-vol");
    let f = t.file("a.txt", b"abc");
    let mut r = request(&f);
    let c_guid = strata_clean::volume::volume_info(
        &strata_clean::canon::CanonicalPath::parse(r"C:\Windows").unwrap(),
    )
    .unwrap()
    .guid
    .unwrap();
    if f.display().to_string().starts_with('C') {
        return;
    }
    r.volume = c_guid;
    assert!(verify_and_delete_by_id(guard(), &r, &CancelToken::new()).is_err());
    assert!(f.exists());
}

#[test]
fn delayed_delete_validation_on_temp_files() {
    let t = TestDir::new("priv-delay");
    let f = t.file("stubborn.tmp", b"s");
    let c = guard().check_path(&f).unwrap();
    let ok = DelayedDeleteRequest {
        path: f.display().to_string(),
        file_ref: FileRef(c.facts.identity.file_index),
        expected_size: c.facts.size,
        expected_mtime: c.facts.modified,
    };
    assert!(ok.validate(guard()).is_ok());

    let mut stale = ok.clone();
    stale.expected_size = 99;
    assert!(matches!(
        stale.validate(guard()),
        Err(CleanError::Changed { .. })
    ));

    let d = t.dir("dir");
    let dc = guard().check_path(&d).unwrap();
    let dir_req = DelayedDeleteRequest {
        path: d.display().to_string(),
        file_ref: FileRef(dc.facts.identity.file_index),
        expected_size: 0,
        expected_mtime: dc.facts.modified,
    };
    assert!(dir_req.validate(guard()).is_err());

    let hl = t.path.join("second-name.tmp");
    std::fs::hard_link(&f, &hl).unwrap();
    let c2 = guard().check_path(&f).unwrap();
    let linked = DelayedDeleteRequest {
        expected_mtime: c2.facts.modified,
        ..ok
    };
    assert!(matches!(
        linked.validate(guard()),
        Err(CleanError::Refused { .. })
    ));
}
