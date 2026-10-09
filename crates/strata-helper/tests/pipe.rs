//! The request loop end to end over a real named pipe, unelevated: the
//! helper runs in-process and scans synthetic NTFS images through the
//! debug-only image source.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::{HelperSetup, TestDir, TestHelper, image_with_files, write_image};
use strata_core::{EntryFlags, FileRef};
use strata_helper::client::{ClientError, HelperClient, ScanEvent};
use strata_helper::server::ExitReason;
use strata_helper::source::IMAGE_VOLUME;
use strata_ipc::protocol::{
    AuditOp, AuditPhase, ErrorCode, HandshakeReject, PROTOCOL_VERSION, ScanOptions, ScanStats,
};
use strata_ipc::rate::RateLimit;
use strata_ntfs::NtfsVolume;

fn image_helper(dir: &TestDir, files: u64) -> (TestHelper, Vec<u8>) {
    let image = image_with_files(files);
    let path = write_image(dir, &image);
    let helper = TestHelper::start(HelperSetup {
        image: Some(path),
        ..HelperSetup::default()
    });
    (helper, image)
}

fn reference_ids(image: Vec<u8>) -> BTreeSet<u64> {
    let vol = NtfsVolume::open(image).unwrap();
    let mut ids = BTreeSet::new();
    vol.scan(&strata_ntfs::ScanOptions::default(), |b| {
        ids.extend(b.iter().map(|r| r.id.0));
    })
    .unwrap();
    ids
}

struct Collected {
    ids: Vec<u64>,
    batches: Vec<usize>,
    progress: usize,
    audit: Vec<AuditPhase>,
    stats: ScanStats,
    metadata: usize,
}

fn collect(stream: strata_helper::client::ScanStream) -> Collected {
    let mut c = Collected {
        ids: Vec::new(),
        batches: Vec::new(),
        progress: 0,
        audit: Vec::new(),
        stats: ScanStats::default(),
        metadata: 0,
    };
    let mut done = false;
    for event in stream {
        assert!(!done, "events after Done");
        match event.unwrap() {
            ScanEvent::Batch(records) => {
                c.batches.push(records.len());
                c.metadata += records
                    .iter()
                    .filter(|r| r.flags.contains(EntryFlags::NTFS_METADATA))
                    .count();
                c.ids.extend(records.iter().map(|r| r.id.0));
            }
            ScanEvent::Progress(_) => c.progress += 1,
            ScanEvent::Audit(a) => {
                assert_eq!(a.op, AuditOp::ScanVolume);
                c.audit.push(a.phase);
            }
            ScanEvent::Done(stats) => {
                c.stats = stats;
                done = true;
            }
        }
    }
    assert!(done, "stream ended without Done");
    c
}

#[test]
fn handshake_ping_and_volumes() {
    let dir = TestDir::new("hello");
    let (helper, _) = image_helper(&dir, 4);
    let client = helper.connect();
    assert_eq!(client.welcome().protocol, PROTOCOL_VERSION);
    assert!(client.welcome().capabilities.mft_scan);
    assert!(client.ping().unwrap() < Duration::from_secs(5));
    let volumes = client.list_volumes().unwrap();
    assert!(
        volumes
            .iter()
            .any(|v| v.guid_path.as_deref() == Some(IMAGE_VOLUME))
    );
    assert!(volumes.len() > 1, "real volumes are listed too");
}

#[test]
fn scan_streams_every_record_in_bounded_batches() {
    let dir = TestDir::new("scan");
    let (helper, image) = image_helper(&dir, 3000);
    let client = helper.connect();
    let options = ScanOptions {
        batch_size: 100,
        progress_interval_ms: 50,
        include_metadata: true,
    };
    let got = collect(client.scan_volume(IMAGE_VOLUME, options).unwrap());
    let reference = reference_ids(image);
    assert_eq!(got.ids.len(), reference.len(), "no duplicates, no losses");
    assert_eq!(got.ids.iter().copied().collect::<BTreeSet<_>>(), reference);
    assert!(got.batches.iter().all(|&n| n <= 100), "{:?}", got.batches);
    assert!(got.batches.len() >= 30);
    assert!(got.progress >= 1, "final progress is always sent");
    assert_eq!(got.audit, [AuditPhase::Started, AuditPhase::Succeeded]);
    assert_eq!(got.stats.records, reference.len() as u64);
    assert!(!got.stats.cancelled);
    assert!(got.metadata > 0);
}

#[test]
fn metadata_can_be_excluded() {
    let dir = TestDir::new("meta");
    let (helper, _) = image_helper(&dir, 10);
    let client = helper.connect();
    let options = ScanOptions {
        include_metadata: false,
        ..ScanOptions::default()
    };
    let got = collect(client.scan_volume(IMAGE_VOLUME, options).unwrap());
    assert_eq!(got.metadata, 0);
    assert!(got.ids.contains(&FileRef::from_parts(16, 1).0));
    assert_eq!(got.stats.records, got.ids.len() as u64);
}

#[test]
fn slow_client_backpressure_then_cancel_mid_scan() {
    let dir = TestDir::new("cancel");
    let (helper, image) = image_helper(&dir, 20_000);
    let total = reference_ids(image).len();
    let client = helper.connect();
    let options = ScanOptions {
        batch_size: 64,
        ..ScanOptions::default()
    };
    let stream = client.scan_volume(IMAGE_VOLUME, options).unwrap();
    // NOTE: not draining the stream: with end-to-end backpressure the helper
    // must stall instead of buffering the whole volume.
    std::thread::sleep(Duration::from_millis(1500));
    // A second scan is refused while the first holds the scanner.
    let second = client.scan_volume(IMAGE_VOLUME, options).unwrap();
    stream.cancel().unwrap();
    let got = collect(stream);
    assert!(
        got.stats.cancelled,
        "the helper finished despite backpressure"
    );
    assert!(got.ids.len() < total, "{} of {total}", got.ids.len());
    assert_eq!(got.stats.records, got.ids.len() as u64);
    let refused: Vec<_> = second.collect();
    assert!(
        matches!(
            refused.as_slice(),
            [Err(ClientError::Remote(r))] if r.code == ErrorCode::Busy
        ),
        "{refused:?}"
    );
    // The connection stays usable, and a new scan runs to completion.
    client.ping().unwrap();
    let again = collect(
        client
            .scan_volume(IMAGE_VOLUME, ScanOptions::default())
            .unwrap(),
    );
    assert_eq!(again.ids.len(), total);
}

#[test]
fn dropping_a_stream_cancels_the_scan() {
    let dir = TestDir::new("drop");
    let (helper, _) = image_helper(&dir, 20_000);
    let client = helper.connect();
    let mut stream = client
        .scan_volume(
            IMAGE_VOLUME,
            ScanOptions {
                batch_size: 64,
                ..ScanOptions::default()
            },
        )
        .unwrap();
    assert!(stream.next().is_some());
    drop(stream);
    // The scan slot frees up once the helper sees the cancel.
    let mut ok = false;
    for _ in 0..50 {
        let s = client
            .scan_volume(IMAGE_VOLUME, ScanOptions::default())
            .unwrap();
        match collect_result(s) {
            Ok(_) => {
                ok = true;
                break;
            }
            Err(ClientError::Remote(r)) if r.code == ErrorCode::Busy => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(ok);
}

fn collect_result(stream: strata_helper::client::ScanStream) -> Result<ScanStats, ClientError> {
    let mut last = None;
    for e in stream {
        if let ScanEvent::Done(s) = e? {
            last = Some(s);
        }
    }
    last.ok_or(ClientError::Disconnected)
}

#[test]
fn client_disconnect_mid_scan_ends_the_helper_cleanly() {
    let dir = TestDir::new("disc");
    let image = image_with_files(20_000);
    let path = write_image(&dir, &image);
    let mut helper = TestHelper::start(HelperSetup {
        image: Some(path),
        exit_on_disconnect: true,
        ..HelperSetup::default()
    });
    let client = helper.connect();
    let mut stream = client
        .scan_volume(
            IMAGE_VOLUME,
            ScanOptions {
                batch_size: 64,
                ..ScanOptions::default()
            },
        )
        .unwrap();
    assert!(stream.next().is_some());
    // The app goes away without cancelling: the pipe closes under a running
    // scan.
    drop(client);
    let exit = helper.wait_exit(Duration::from_secs(15));
    assert_eq!(exit, Some(Ok(ExitReason::ClientDisconnected)));
    let rest: Vec<_> = stream.by_ref().collect();
    assert!(
        matches!(rest.last(), Some(Err(ClientError::Disconnected)) | None),
        "{:?}",
        rest.last().map(|r| r.is_ok())
    );
}

#[test]
fn rate_limit_is_typed() {
    let helper = TestHelper::start(HelperSetup {
        rate_limit: RateLimit {
            burst: 5,
            per_second: 1,
        },
        ..HelperSetup::default()
    });
    let client = helper.connect();
    let results: Vec<_> = (0..10).map(|_| client.ping()).collect();
    let limited = results
        .iter()
        .filter(|r| matches!(r, Err(ClientError::RateLimited { retry_after }) if !retry_after.is_zero()))
        .count();
    assert!(limited >= 4, "{results:?}");
    assert!(results[0].is_ok());
    std::thread::sleep(Duration::from_millis(1100));
    client.ping().unwrap();
}

#[test]
fn version_mismatch_is_rejected() {
    let helper = TestHelper::start(HelperSetup::default());
    let mut config = helper.client_config();
    config.options.protocol = PROTOCOL_VERSION + 1;
    let err = HelperClient::connect(config).unwrap_err();
    assert_eq!(
        err,
        ClientError::Rejected(HandshakeReject::VersionMismatch {
            helper: PROTOCOL_VERSION,
            client: PROTOCOL_VERSION + 1,
        })
    );
    // The helper keeps serving after a refused handshake.
    helper.connect().ping().unwrap();
}

#[test]
fn only_the_launching_process_may_connect() {
    let helper = TestHelper::start(HelperSetup {
        client_pid: std::process::id().wrapping_add(4),
        ..HelperSetup::default()
    });
    let err = HelperClient::connect(helper.client_config()).unwrap_err();
    assert_eq!(err, ClientError::Rejected(HandshakeReject::Untrusted));
}

#[test]
fn client_refuses_an_unexpected_server() {
    let helper = TestHelper::start(HelperSetup::default());
    let mut config = helper.client_config();
    config.options.server_verifier = Some(std::sync::Arc::new(
        strata_helper::verify::trust_policy(r"C:\Windows\System32\notepad.exe").unwrap(),
    ));
    assert!(matches!(
        HelperClient::connect(config),
        Err(ClientError::Untrusted(_))
    ));
}

#[test]
fn read_records_reports_current_and_stale_references() {
    let dir = TestDir::new("records");
    let (helper, image) = image_helper(&dir, 50);
    let client = helper.connect();
    let vol = NtfsVolume::open(image).unwrap();
    let live = vol.read_record(20).unwrap().unwrap();
    let stale = FileRef::from_parts(21, 9);
    let free = FileRef::from_parts(4000, 1);
    let reply = client
        .read_records(IMAGE_VOLUME, vec![live.id, stale, free])
        .unwrap();
    assert_eq!(reply.records, vec![live]);
    assert_eq!(reply.missing, vec![stale, free]);
    let too_many = vec![FileRef(1); 70_000];
    let err = client.read_records(IMAGE_VOLUME, too_many).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::BadRequest));
}

#[test]
fn malformed_and_unsupported_requests_are_typed() {
    let dir = TestDir::new("bad");
    let (helper, _) = image_helper(&dir, 4);
    let client = helper.connect();
    for volume in [r"C:\Windows", r"\\.\PhysicalDrive0", r"..\x.img", ""] {
        let err = collect_result(client.scan_volume(volume, ScanOptions::default()).unwrap())
            .unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::BadRequest), "{volume}");
    }
    let err = client.query_usn_journal(IMAGE_VOLUME).unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NotSupported));
    if !strata_win::process::is_elevated().unwrap_or(false) {
        let err =
            collect_result(client.scan_volume("C:", ScanOptions::default()).unwrap()).unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::AccessDenied));
        let err = client.query_usn_journal("C:").unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::AccessDenied));
    }
}

#[test]
fn shutdown_ends_the_helper() {
    let mut helper = TestHelper::start(HelperSetup::default());
    let client = helper.connect();
    client.shutdown().unwrap();
    assert_eq!(
        helper.wait_exit(Duration::from_secs(10)),
        Some(Ok(ExitReason::Shutdown))
    );
}

#[test]
fn pipe_mode_reconnects_to_the_same_helper() {
    let helper = TestHelper::start(HelperSetup::default());
    let mut client = helper.connect();
    client.ping().unwrap();
    client.reconnect().unwrap();
    client.ping().unwrap();
    assert!(client.is_connected());
}
