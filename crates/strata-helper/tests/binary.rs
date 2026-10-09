//! The real `strata-helper.exe`, started unelevated with the debug-only
//! `--image` source: argument handling, exit on disconnect, parent death,
//! crash detection and reconnect.

#![cfg(debug_assertions)]

mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{TestDir, image_with_files, write_image};
use strata_helper::client::{ClientConfig, ClientError, HelperClient, ScanEvent};
use strata_helper::run::{EXIT_OK, EXIT_PARENT_GONE, EXIT_USAGE};
use strata_helper::source::IMAGE_VOLUME;
use strata_ipc::protocol::ScanOptions;

fn helper_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_strata-helper"))
}

fn spawn_config(image: &std::path::Path) -> ClientConfig {
    let mut c = ClientConfig::spawn(
        helper_exe(),
        vec![OsString::from("--image"), image.as_os_str().to_owned()],
    );
    c.options.connect_timeout = Duration::from_secs(20);
    c
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match strata_helper::watch::ProcessWatch::open(pid) {
            Ok(w) if w.is_alive() => std::thread::sleep(Duration::from_millis(100)),
            _ => return true,
        }
    }
    false
}

#[test]
fn bad_arguments_exit_with_usage() {
    for args in [
        vec!["--bogus"],
        vec![
            "--pipe",
            r"\\.\pipe\not-ours",
            "--client-image",
            r"C:\x.exe",
            "--client-pid",
            "1",
        ],
        vec!["--service"],
    ] {
        let out = Command::new(helper_exe())
            .args(&args)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(EXIT_USAGE), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage"));
    }
    let out = Command::new(helper_exe())
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
}

#[test]
fn scans_an_image_then_exits_when_the_client_disconnects() {
    let dir = TestDir::new("bin-scan");
    let image = write_image(&dir, &image_with_files(500));
    let client = HelperClient::connect(spawn_config(&image)).unwrap();
    let pid = client.helper_pid().unwrap();
    assert!(!client.welcome().elevated || strata_win::process::is_elevated().unwrap());
    let mut records = 0;
    let mut done = false;
    for e in client
        .scan_volume(IMAGE_VOLUME, ScanOptions::default())
        .unwrap()
    {
        match e.unwrap() {
            ScanEvent::Batch(b) => records += b.len(),
            ScanEvent::Done(s) => {
                assert_eq!(s.records as usize, records);
                done = true;
            }
            _ => {}
        }
    }
    assert!(done && records > 500);
    drop(client);
    assert!(
        wait_for_exit(pid, Duration::from_secs(10)),
        "helper kept running"
    );
}

#[test]
fn a_crashed_helper_surfaces_disconnected_and_reconnect_recovers() {
    let dir = TestDir::new("bin-crash");
    let image = write_image(&dir, &image_with_files(20));
    let mut client = HelperClient::connect(spawn_config(&image)).unwrap();
    client.ping().unwrap();
    let pid = client.helper_pid().unwrap();
    // Simulate a crash of our own child process.
    let status = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(wait_for_exit(pid, Duration::from_secs(10)));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match client.ping() {
            Err(ClientError::Disconnected) => break,
            Ok(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }
    assert!(!client.is_connected());
    client.reconnect().unwrap();
    assert_ne!(client.helper_pid(), Some(pid));
    client.ping().unwrap();
}

#[test]
fn helper_exits_when_its_parent_dies() {
    let dir = TestDir::new("bin-parent");
    let image = write_image(&dir, &image_with_files(4));
    let mut parent = Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let sid = strata_win::process::current_user().unwrap().sid;
    let pipe = strata_ipc::security::session_pipe_name(&sid).unwrap();
    let mut helper = Command::new(helper_exe())
        .args(["--pipe", &pipe, "--client-image"])
        .arg(std::env::current_exe().unwrap())
        .args(["--client-pid", &std::process::id().to_string()])
        .args(["--parent-pid", &parent.id().to_string(), "--image"])
        .arg(&image)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(helper.try_wait().unwrap().is_none());
    parent.kill().unwrap();
    parent.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = helper.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "helper outlived its parent");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.code(), Some(EXIT_PARENT_GONE));
}
