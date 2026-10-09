//! The app's helper client against an in-process fake helper on a real,
//! secured named pipe: a full `ScanVolume` stream into the index, cancel,
//! and a helper that disappears mid-scan.

#![cfg(debug_assertions)]

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use strata_app_lib::classify::Engine;
use strata_app_lib::helper::scan_volume;
use strata_app_lib::model::ScannerUsed;
use strata_app_lib::scan::{DataSlot, Ingest, ScanObserver, ScanProgress, ScanTarget, ShadowBytes};
use strata_core::known::KnownFolders;
use strata_core::{EntryFlags, FileRef, NameLink, ScanRecord, SizeMode, Sizes, Times, WideName};
use strata_helper::client::{ClientConfig, HelperClient};
use strata_ipc::pipe::{ClientOptions, PipeServer, ServerConfig, ServerConnection};
use strata_ipc::protocol::{Request, Response, ScanStats};
use strata_ipc::security::{PeerVerifier, TrustPolicy, session_pipe_name};

fn rec(id: u64, parent: u64, name: &str, size: u64, dir: bool) -> ScanRecord {
    let r = |n| FileRef::from_parts(n, 1);
    ScanRecord {
        id: r(id),
        links: vec![NameLink {
            parent: r(parent),
            name: WideName::from_str_lossless(name),
        }],
        attributes: 0,
        flags: if dir {
            EntryFlags::DIR
        } else {
            EntryFlags::EMPTY
        },
        times: Times::default(),
        fn_created: None,
        sizes: Sizes {
            logical: size,
            allocated: size,
            ..Sizes::default()
        },
        reparse: None,
        ads: vec![],
    }
}

fn policy() -> Arc<dyn PeerVerifier> {
    Arc::new(TrustPolicy::dev_unsigned(std::env::current_exe().unwrap()).unwrap())
}

/// Starts a fake helper; `serve` handles the one connection.
fn fake_helper(
    serve: impl FnOnce(ServerConnection) + Send + 'static,
) -> (std::thread::JoinHandle<()>, String) {
    let sid = strata_win::process::current_user().unwrap().sid;
    let name = session_pipe_name(&sid).unwrap();
    let mut config = ServerConfig::new(&name, sid, policy());
    config.capabilities.mft_scan = true;
    let mut server = PipeServer::create(config).unwrap();
    let h = std::thread::spawn(move || {
        let conn = server.accept(Some(Duration::from_secs(10))).unwrap();
        serve(conn);
    });
    (h, name)
}

fn connect(name: &str) -> HelperClient {
    let mut config = ClientConfig::pipe(name);
    config.options = ClientOptions {
        connect_timeout: Duration::from_secs(5),
        server_verifier: Some(policy()),
        ..ClientOptions::default()
    };
    HelperClient::connect(config).unwrap()
}

struct Quiet;
impl ScanObserver for Quiet {
    fn progress(&self, _: &ScanProgress) {}
}

fn target() -> ScanTarget {
    ScanTarget {
        root: r"X:\".into(),
        root_display: r"X:\".into(),
        volume: None,
    }
}

fn wait_scan(conn: &ServerConnection) -> u32 {
    loop {
        if let Some((id, Request::ScanVolume { .. })) =
            conn.recv_request(Some(Duration::from_secs(5))).unwrap()
        {
            return id;
        }
    }
}

#[test]
fn scan_stream_builds_the_index() {
    let (server, name) = fake_helper(|conn| {
        let id = wait_scan(&conn);
        let mut batch = vec![rec(5, 5, "", 0, true), rec(20, 5, "dir", 0, true)];
        batch.extend((0..1000).map(|i| rec(100 + i, 20, &format!("f{i}"), 10, false)));
        for chunk in batch.chunks(300) {
            conn.send(
                id,
                Response::ScanBatch {
                    records: chunk.to_vec(),
                },
            )
            .unwrap();
        }
        conn.send(
            id,
            Response::ScanDone {
                stats: ScanStats {
                    records: 1002,
                    ..ScanStats::default()
                },
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
    });
    let client = connect(&name);
    let engine = Arc::new(Engine::new(&KnownFolders::default(), None).unwrap());
    let slot: Arc<DataSlot> = Arc::new(RwLock::new(None));
    let mut ingest = Ingest::new(engine, target(), slot.clone(), ScannerUsed::Mft, false);
    let stats = scan_volume(&client, "vol", &mut ingest, &AtomicBool::new(false), &Quiet).unwrap();
    assert!(!stats.cancelled);
    ingest.finish(false, ShadowBytes::default()).unwrap();
    let g = slot.read().unwrap();
    let d = g.as_ref().unwrap();
    assert_eq!(d.scanner, ScannerUsed::Mft);
    assert_eq!(d.index.size(d.index.root(), SizeMode::Logical), 10_000);
    server.join().unwrap();
}

#[test]
fn cancel_is_forwarded() {
    let (server, name) = fake_helper(|conn| {
        let scan = wait_scan(&conn);
        conn.send(
            scan,
            Response::ScanBatch {
                records: vec![rec(5, 5, "", 0, true)],
            },
        )
        .unwrap();
        loop {
            if let Some((id, Request::Cancel { request_id })) =
                conn.recv_request(Some(Duration::from_secs(5))).unwrap()
            {
                assert_eq!(request_id, scan);
                conn.send(id, Response::CancelAck { was_running: true })
                    .unwrap();
                conn.send(
                    scan,
                    Response::ScanDone {
                        stats: ScanStats {
                            cancelled: true,
                            ..ScanStats::default()
                        },
                    },
                )
                .unwrap();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    });
    let client = connect(&name);
    let engine = Arc::new(Engine::new(&KnownFolders::default(), None).unwrap());
    let slot: Arc<DataSlot> = Arc::new(RwLock::new(None));
    let mut ingest = Ingest::new(engine, target(), slot.clone(), ScannerUsed::Mft, false);
    let stats = scan_volume(&client, "vol", &mut ingest, &AtomicBool::new(true), &Quiet).unwrap();
    assert!(stats.cancelled);
    server.join().unwrap();
}

#[test]
fn helper_crash_keeps_the_previous_index() {
    let (server, name) = fake_helper(|conn| {
        let id = wait_scan(&conn);
        conn.send(
            id,
            Response::ScanBatch {
                records: vec![rec(5, 5, "", 0, true)],
            },
        )
        .unwrap();
        // The helper "crashes": the connection drops without ScanDone.
        drop(conn);
    });
    let client = connect(&name);
    let engine = Arc::new(Engine::new(&KnownFolders::default(), None).unwrap());
    let slot: Arc<DataSlot> = Arc::new(RwLock::new(None));
    let mut ingest = Ingest::new(engine, target(), slot.clone(), ScannerUsed::Mft, false);
    let err =
        scan_volume(&client, "vol", &mut ingest, &AtomicBool::new(false), &Quiet).unwrap_err();
    assert!(err.to_string().contains("disconnected"), "{err}");
    drop(ingest);
    assert!(
        slot.read().unwrap().is_none(),
        "nothing half-built was published"
    );
    server.join().unwrap();
    for _ in 0..50 {
        if !client.is_connected() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!client.is_connected());
}
