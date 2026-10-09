//! Restart Manager lock detection against a real child process.

mod common;

use common::{Holder, TestDir};
use strata_clean::locks::{DEFAULT_MAX_FILES, who_locks};

#[test]
fn locked_file_names_the_process() {
    let t = TestDir::new("lock-file");
    let f = t.file("locked.bin", b"data");
    let holder = Holder::spawn(&f);
    let holders = who_locks(&f, DEFAULT_MAX_FILES).unwrap();
    let h = holders
        .iter()
        .find(|h| h.pid == holder.0.id())
        .unwrap_or_else(|| panic!("child not reported: {holders:?}"));
    let exe = h.exe_path.as_deref().unwrap_or_default().to_lowercase();
    assert!(
        exe.ends_with("powershell.exe") || h.app_name.to_lowercase().contains("powershell"),
        "{h:?}"
    );
    assert!(h.start_time > 0);
}

#[test]
fn locked_file_inside_folder_is_found() {
    let t = TestDir::new("lock-dir");
    t.file(r"a\one.txt", b"1");
    let f = t.file(r"a\b\two.txt", b"2");
    let holder = Holder::spawn(&f);
    let holders = who_locks(&t.path.join("a"), DEFAULT_MAX_FILES).unwrap();
    assert!(
        holders.iter().any(|h| h.pid == holder.0.id()),
        "{holders:?}"
    );
}

#[test]
fn unlocked_items_have_no_holders() {
    let t = TestDir::new("lock-none");
    let f = t.file("free.txt", b"x");
    assert!(who_locks(&f, DEFAULT_MAX_FILES).unwrap().is_empty());
    assert!(who_locks(&t.path, DEFAULT_MAX_FILES).unwrap().is_empty());
}
