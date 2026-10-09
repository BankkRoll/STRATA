//! Real ETW sessions. Kernel tracing needs an elevated process:
//!
//! - `unelevated_*` run everywhere and check that an unelevated process is
//!   refused cleanly (skipped when elevated).
//! - The `#[ignore]` tests run a real session and skip with a message when
//!   not elevated. Run them from an elevated terminal:
//!   `cargo test -p strata-etw --test live_session -- --ignored --nocapture`
//!
//! Every session started here uses a test-only name and is owned by a guard
//! or a `Monitor`, so a failing assertion still stops it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use strata_etw::error::code;
use strata_etw::ffi::Win32Control;
use strata_etw::session::{
    SessionConfig, SessionGuard, TraceControl, recover_orphaned_session_with,
};
use strata_etw::{EtwError, Monitor, MonitorConfig, MonitorMessage, StopReason, Window};
use strata_store::SystemClock;
use strata_win::path::DeviceMap;

const TEST_SESSION: &str = "Strata-FileActivity-Test";

fn elevated() -> bool {
    strata_win::process::is_elevated().unwrap_or(false)
}

fn test_config() -> MonitorConfig {
    let mut cfg = MonitorConfig::default();
    cfg.session.name = TEST_SESSION.into();
    cfg.flush_interval = Duration::from_secs(1);
    cfg.overhead_interval = Duration::from_millis(500);
    cfg
}

#[test]
fn unelevated_start_is_refused_and_leaves_nothing() {
    if elevated() {
        eprintln!("skipped: process is elevated; see the ignored live tests");
        return;
    }
    let err =
        Monitor::start(test_config(), DeviceMap::default(), Arc::new(SystemClock)).unwrap_err();
    assert_eq!(err, EtwError::NotElevated);
    // The raw control path reports access denied as NotElevated too; if the
    // account may create sessions (Performance Log Users), the guard stops
    // the one it made.
    let cfg = SessionConfig {
        name: TEST_SESSION.into(),
        ..SessionConfig::default()
    };
    match SessionGuard::start(Win32Control, &cfg) {
        Err(EtwError::NotElevated) => {}
        Err(e) => panic!("unexpected error {e}"),
        Ok(mut g) => {
            g.stop().unwrap();
        }
    }
    match recover_orphaned_session_with(&mut Win32Control, TEST_SESSION) {
        Ok(false) | Err(EtwError::NotElevated) => {}
        other => panic!("{other:?}"),
    }
}

fn canonical_dir(p: &std::path::Path) -> String {
    let c = std::fs::canonicalize(p).unwrap();
    let s = c.to_string_lossy().into_owned();
    s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s)
}

#[test]
#[ignore = "needs an elevated process; starts a real kernel trace session"]
fn live_session_attributes_this_process() {
    if !elevated() {
        eprintln!("skipped: not elevated (run from an elevated terminal with --ignored)");
        return;
    }
    let base = std::env::temp_dir().join(format!("strata-etw-live-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let dir = canonical_dir(&base);
    let (monitor, rx) = Monitor::start(
        test_config(),
        DeviceMap::current().unwrap(),
        Arc::new(SystemClock),
    )
    .unwrap();
    assert!(
        monitor.manifest_errors().is_empty(),
        "{:?}",
        monitor.manifest_errors()
    );

    // Give the providers a moment to attach, then generate activity.
    std::thread::sleep(Duration::from_millis(500));
    let payload = vec![0x5Au8; 3 << 20];
    let part = base.join("download.part");
    std::fs::write(&part, &payload).unwrap();
    std::fs::rename(&part, base.join("download.bin")).unwrap();
    std::fs::write(base.join("scratch.tmp"), b"x").unwrap();
    std::fs::remove_file(base.join("scratch.tmp")).unwrap();

    let me = std::env::current_exe().unwrap();
    let me = canonical_dir(&me);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mine = loop {
        let rows = monitor.dir_totals(Window::LastHour, 1000);
        let hit = rows
            .into_iter()
            .find(|r| r.dir.eq_ignore_ascii_case(&dir) && r.image.eq_ignore_ascii_case(&me));
        match hit {
            Some(h) if h.counts.files_deleted >= 1 => break h,
            _ if Instant::now() > deadline => {
                panic!("no activity seen for {dir}; stats {:?}", monitor.stats())
            }
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    };
    eprintln!("observed: {mine:?}; stats {:?}", monitor.stats());
    assert!(mine.counts.bytes_written >= payload.len() as u64);
    assert!(mine.counts.files_created >= 2);

    let session = monitor.stop().unwrap();
    eprintln!("session: {session:?}");
    let msgs: Vec<MonitorMessage> = rx.try_iter().collect();
    let batch_rows: usize = msgs
        .iter()
        .filter_map(|m| match m {
            MonitorMessage::Batch(b) => Some(b.samples.len()),
            _ => None,
        })
        .sum();
    assert!(batch_rows > 0);
    let last_writes: Vec<u64> = msgs
        .iter()
        .filter_map(|m| match m {
            MonitorMessage::Batch(b) => Some(b.last_writes.iter().map(|w| w.path_hash)),
            _ => None,
        })
        .flatten()
        .collect();
    let final_name = format!(r"{dir}\download.bin");
    assert!(
        last_writes.contains(&strata_store::path_hash(&final_name)),
        "last writer did not follow the rename to {final_name}"
    );
    assert!(matches!(
        msgs.last(),
        Some(MonitorMessage::Stopped {
            reason: StopReason::Requested,
            ..
        })
    ));
    assert_eq!(Win32Control.exists(TEST_SESSION), Ok(false));
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
#[ignore = "needs an elevated process; starts a real kernel trace session"]
fn live_crash_recovery_and_stale_takeover() {
    if !elevated() {
        eprintln!("skipped: not elevated (run from an elevated terminal with --ignored)");
        return;
    }
    let cfg = SessionConfig {
        name: TEST_SESSION.into(),
        ..SessionConfig::default()
    };
    // Simulate a crash: the guard is leaked, so nothing stops the session.
    std::mem::forget(SessionGuard::start(Win32Control, &cfg).unwrap());
    assert_eq!(Win32Control.exists(TEST_SESSION), Ok(true));
    assert!(recover_orphaned_session_with(&mut Win32Control, TEST_SESSION).unwrap());
    assert_eq!(Win32Control.exists(TEST_SESSION), Ok(false));

    // A stale session is taken over by the next start.
    std::mem::forget(SessionGuard::start(Win32Control, &cfg).unwrap());
    {
        let g = SessionGuard::start(Win32Control, &cfg).unwrap();
        assert!(g.is_live());
    }
    assert_eq!(Win32Control.exists(TEST_SESSION), Ok(false));
    assert_eq!(Win32Control.stop(TEST_SESSION), Err(code::NOT_FOUND));
}
