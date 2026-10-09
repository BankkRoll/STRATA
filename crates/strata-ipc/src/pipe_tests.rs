//! End-to-end tests on real named pipes. The test binary is both server and
//! client, so peer verification runs against itself through the injectable
//! policy.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use strata_core::{EntryFlags, FileRef, FileTime, NameLink, ScanRecord, Sizes, Times, WideName};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
};
use windows::core::PWSTR;

use super::*;
use crate::frame::{FrameError, MAX_FRAME_LEN};
use crate::protocol::{ScanOptions, ScanStats};
#[cfg(debug_assertions)]
use crate::security::TrustPolicy;
use crate::security::session_pipe_name;

fn sid() -> String {
    strata_win::process::current_user().unwrap().sid
}

/// The real development policy: the test binary is unsigned, so the peer
/// must be an unsigned process running `image`.
#[cfg(debug_assertions)]
fn policy_for(image: impl Into<std::path::PathBuf>) -> Arc<dyn PeerVerifier> {
    Arc::new(TrustPolicy::dev_unsigned(image).unwrap())
}

/// Release builds (e.g. `cargo bench`, `cargo test --release`) cannot
/// construct `TrustPolicy::DevUnsigned`, so the unsigned test binary is
/// checked by image path only.
#[cfg(not(debug_assertions))]
fn policy_for(image: impl Into<std::path::PathBuf>) -> Arc<dyn PeerVerifier> {
    #[derive(Debug)]
    struct ImageOnly(std::path::PathBuf);
    impl PeerVerifier for ImageOnly {
        fn verify(&self, peer: &PeerIdentity) -> Result<(), TrustError> {
            if crate::security::same_path(&self.0, &peer.image) {
                Ok(())
            } else {
                Err(TrustError::ImageMismatch {
                    expected: self.0.clone(),
                    actual: peer.image.clone(),
                })
            }
        }
    }
    Arc::new(ImageOnly(image.into()))
}

fn self_policy() -> Arc<dyn PeerVerifier> {
    policy_for(std::env::current_exe().unwrap())
}

fn server_with(f: impl FnOnce(&mut ServerConfig)) -> (PipeServer, String) {
    let sid = sid();
    let name = session_pipe_name(&sid).unwrap();
    let mut config = ServerConfig::new(&name, sid, self_policy());
    config.capabilities.mft_scan = true;
    f(&mut config);
    (PipeServer::create(config).unwrap(), name)
}

fn client_opts() -> ClientOptions {
    ClientOptions {
        connect_timeout: Duration::from_secs(5),
        handshake_timeout: Duration::from_secs(5),
        server_verifier: Some(self_policy()),
        ..Default::default()
    }
}

fn record(i: u64) -> ScanRecord {
    ScanRecord {
        id: FileRef::from_parts(i + 16, 1),
        links: vec![NameLink {
            parent: FileRef::from_parts(5, 5),
            name: WideName::from_str_lossless(&format!("file-{i:08}.bin")),
        }],
        attributes: 0x20,
        flags: EntryFlags::EMPTY,
        times: Times {
            created: FileTime(133_000_000_000_000_000 + i),
            modified: FileTime(133_000_000_000_000_000 + 2 * i),
            accessed: FileTime(133_000_000_000_000_000),
            changed: FileTime(0),
        },
        fn_created: None,
        sizes: Sizes {
            logical: i * 1000,
            allocated: (i * 1000).div_ceil(4096) * 4096,
            ..Default::default()
        },
        reparse: None,
        ads: vec![],
    }
}

/// Accepts one client and answers pings until it leaves.
fn ping_server(server: &mut PipeServer) -> Result<(), IpcError> {
    let conn = server.accept(Some(Duration::from_secs(5)))?;
    loop {
        match conn.recv_request(None) {
            Ok(Some((id, Request::Ping))) => conn.send(id, Response::Pong)?,
            Ok(Some((id, _))) => conn.send(
                id,
                Response::Error(ErrorReply {
                    code: ErrorCode::NotSupported,
                    message: String::new(),
                    retry_after_ms: None,
                }),
            )?,
            Ok(None) => {}
            Err(IpcError::Disconnected) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

#[test]
fn handshake_and_ping() {
    let (mut server, name) = server_with(|c| c.elevated = true);
    std::thread::scope(|s| {
        let srv = s.spawn(|| ping_server(&mut server));
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        assert_eq!(client.welcome().protocol, PROTOCOL_VERSION);
        assert!(client.welcome().elevated);
        assert!(client.welcome().capabilities.mft_scan);
        assert_eq!(client.server().unwrap().pid, std::process::id());
        for _ in 0..3 {
            assert_eq!(
                client
                    .call(Request::Ping, Some(Duration::from_secs(5)))
                    .unwrap(),
                Response::Pong
            );
        }
        let r = client
            .call(Request::ListVolumes, Some(Duration::from_secs(5)))
            .unwrap();
        assert!(matches!(
            r,
            Response::Error(ErrorReply {
                code: ErrorCode::NotSupported,
                ..
            })
        ));
        drop(client);
        srv.join().unwrap().unwrap();
    });
}

#[test]
fn version_mismatch_is_rejected_with_a_typed_error() {
    let (mut server, name) = server_with(|_| {});
    std::thread::scope(|s| {
        let srv = s.spawn(|| server.accept(Some(Duration::from_secs(5))).map(|_| ()));
        let opts = ClientOptions {
            protocol: PROTOCOL_VERSION + 1,
            ..client_opts()
        };
        let err = PipeClient::connect(&name, opts).unwrap_err();
        let expected = HandshakeReject::VersionMismatch {
            helper: PROTOCOL_VERSION,
            client: PROTOCOL_VERSION + 1,
        };
        assert_eq!(err, IpcError::Rejected(expected.clone()));
        assert_eq!(srv.join().unwrap(), Err(IpcError::Rejected(expected)));
    });
}

#[test]
fn untrusted_client_image_is_refused() {
    let notepad =
        std::path::PathBuf::from(std::env::var_os("windir").unwrap()).join(r"System32\notepad.exe");
    let (mut server, name) = server_with(|c| {
        c.verifier = policy_for(&notepad);
    });
    std::thread::scope(|s| {
        let srv = s.spawn(|| server.accept(Some(Duration::from_secs(5))).map(|_| ()));
        let err = PipeClient::connect(&name, client_opts()).unwrap_err();
        assert_eq!(err, IpcError::Rejected(HandshakeReject::Untrusted));
        match srv.join().unwrap() {
            Err(IpcError::Untrusted(TrustError::ImageMismatch { expected, .. })) => {
                assert_eq!(expected, notepad);
            }
            other => panic!("{other:?}"),
        }
    });
    // The pipe is reusable after a rejection.
    std::thread::scope(|s| {
        server.config.verifier = self_policy();
        let srv = s.spawn(|| ping_server(&mut server));
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        assert_eq!(client.call(Request::Ping, None).unwrap(), Response::Pong);
        drop(client);
        srv.join().unwrap().unwrap();
    });
}

#[test]
fn client_refuses_an_unexpected_server() {
    let (mut server, name) = server_with(|_| {});
    std::thread::scope(|s| {
        let srv = s.spawn(|| server.accept(Some(Duration::from_secs(5))).map(|_| ()));
        let opts = ClientOptions {
            server_verifier: Some(policy_for(r"C:\Program Files\Strata\strata-helper.exe")),
            ..client_opts()
        };
        assert!(matches!(
            PipeClient::connect(&name, opts),
            Err(IpcError::Untrusted(TrustError::ImageMismatch { .. }))
        ));
        // The server sees the client hang up during the handshake.
        assert!(srv.join().unwrap().is_err());
    });
}

#[test]
fn second_client_is_busy_and_reconnect_works() {
    let (mut server, name) = server_with(|_| {});
    for _ in 0..2 {
        std::thread::scope(|s| {
            let srv = s.spawn(|| ping_server(&mut server));
            let first = PipeClient::connect(&name, client_opts()).unwrap();
            let opts = ClientOptions {
                connect_timeout: Duration::from_millis(300),
                ..client_opts()
            };
            assert_eq!(
                PipeClient::connect(&name, opts).unwrap_err(),
                IpcError::Busy
            );
            assert_eq!(first.call(Request::Ping, None).unwrap(), Response::Pong);
            drop(first);
            srv.join().unwrap().unwrap();
        });
    }
}

#[test]
fn accept_times_out_without_a_client() {
    let (mut server, _name) = server_with(|_| {});
    let t = Instant::now();
    assert_eq!(
        server.accept(Some(Duration::from_millis(100))).unwrap_err(),
        IpcError::Timeout
    );
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn connect_times_out_without_a_server() {
    let name = session_pipe_name(&sid()).unwrap();
    let opts = ClientOptions {
        connect_timeout: Duration::from_millis(100),
        ..client_opts()
    };
    assert_eq!(
        PipeClient::connect(&name, opts).unwrap_err(),
        IpcError::Timeout
    );
}

/// Streams `total` records in `batch` sized `ScanBatch`es, honoring `Cancel`
/// from a concurrent reader (full duplex on one connection).
fn stream_server(server: &mut PipeServer, total: u64, batch: u64) -> Result<ScanStats, IpcError> {
    let conn = server.accept(Some(Duration::from_secs(5)))?;
    let Some((scan_id, Request::ScanVolume { .. })) = conn.recv_request(None)? else {
        return Err(IpcError::Unexpected("expected ScanVolume"));
    };
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let start = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            while let Ok(Some((id, req))) = conn.recv_request(None) {
                if let Request::Cancel { request_id } = req {
                    cancelled.store(request_id == scan_id, std::sync::atomic::Ordering::SeqCst);
                    let _ = conn.send(id, Response::CancelAck { was_running: true });
                }
            }
        });
        let mut sent = 0u64;
        while sent < total && !cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            let n = batch.min(total - sent);
            let records = (sent..sent + n).map(record).collect();
            conn.send(scan_id, Response::ScanBatch { records })?;
            sent += n;
        }
        let stats = ScanStats {
            records: sent,
            corrupt: 0,
            elapsed_ms: start.elapsed().as_millis() as u64,
            cancelled: cancelled.load(std::sync::atomic::Ordering::SeqCst),
        };
        conn.send(scan_id, Response::ScanDone { stats })?;
        Ok(stats)
        // NOTE: the reader thread exits when the client disconnects.
    })
}

fn scan_request() -> Request {
    Request::ScanVolume {
        volume: r"\\?\Volume{test}\".into(),
        options: ScanOptions::default(),
    }
}

#[test]
fn streams_one_million_records() {
    const TOTAL: u64 = 1_000_000;
    const BATCH: u64 = 8192;
    let (mut server, name) = server_with(|_| {});
    let batch_bytes = {
        let mut buf = Vec::new();
        crate::frame::encode_frame(
            &Envelope {
                request_id: 1,
                message: Message::Response(Response::ScanBatch {
                    records: (0..BATCH).map(record).collect(),
                }),
            },
            &mut buf,
        )
        .unwrap();
        buf.len()
    };
    std::thread::scope(|s| {
        let srv = s.spawn(|| stream_server(&mut server, TOTAL, BATCH));
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        let start = Instant::now();
        let id = client.send(scan_request()).unwrap();
        let mut got = 0u64;
        let mut next_expected = 0u64;
        let stats = loop {
            match client.recv(Some(Duration::from_secs(60))).unwrap().unwrap() {
                (rid, Response::ScanBatch { records }) => {
                    assert_eq!(rid, id);
                    assert_eq!(records[0], record(next_expected));
                    next_expected += records.len() as u64;
                    got += records.len() as u64;
                }
                (rid, Response::ScanDone { stats }) => {
                    assert_eq!(rid, id);
                    break stats;
                }
                other => panic!("{other:?}"),
            }
        };
        let secs = start.elapsed().as_secs_f64();
        assert_eq!(got, TOTAL);
        assert_eq!(stats.records, TOTAL);
        assert!(!stats.cancelled);
        let bytes = batch_bytes as f64 * (TOTAL as f64 / BATCH as f64);
        println!(
            "pipe throughput ({} build): {TOTAL} records in {secs:.2}s = {:.0} records/s, {:.1} MB/s ({:.1} B/record)",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            TOTAL as f64 / secs,
            bytes / secs / 1e6,
            bytes / TOTAL as f64,
        );
        drop(client);
        srv.join().unwrap().unwrap();
    });
}

#[test]
fn cancel_stops_a_stream_mid_way() {
    let (mut server, name) = server_with(|_| {});
    std::thread::scope(|s| {
        let srv = s.spawn(|| stream_server(&mut server, u64::MAX, 1024));
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        let id = client.send(scan_request()).unwrap();
        for _ in 0..3 {
            assert!(matches!(
                client.recv(None).unwrap().unwrap(),
                (rid, Response::ScanBatch { .. }) if rid == id
            ));
        }
        let cancel_id = client.send(Request::Cancel { request_id: id }).unwrap();
        let (mut acked, mut done) = (false, None);
        while done.is_none() || !acked {
            match client.recv(Some(Duration::from_secs(10))).unwrap().unwrap() {
                (rid, Response::CancelAck { .. }) => {
                    assert_eq!(rid, cancel_id);
                    acked = true;
                }
                (_, Response::ScanBatch { .. }) => {}
                (rid, Response::ScanDone { stats }) => {
                    assert_eq!(rid, id);
                    done = Some(stats);
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(done.unwrap().cancelled);
        drop(client);
        assert!(srv.join().unwrap().unwrap().cancelled);
    });
}

#[test]
fn client_disconnect_mid_stream_is_detected_by_the_server() {
    let (mut server, name) = server_with(|_| {});
    std::thread::scope(|s| {
        let srv = s.spawn(|| stream_server(&mut server, u64::MAX, 4096));
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        client.send(scan_request()).unwrap();
        for _ in 0..2 {
            client.recv(None).unwrap().unwrap();
        }
        drop(client);
        assert_eq!(srv.join().unwrap().unwrap_err(), IpcError::Disconnected);
    });
}

#[test]
fn server_disconnect_mid_stream_is_detected_by_the_client() {
    let (mut server, name) = server_with(|_| {});
    std::thread::scope(|s| {
        let srv = s.spawn(|| -> Result<(), IpcError> {
            let conn = server.accept(Some(Duration::from_secs(5)))?;
            let (id, _) = conn.recv_request(None)?.unwrap();
            for i in 0..3 {
                conn.send(
                    id,
                    Response::ScanBatch {
                        records: vec![record(i)],
                    },
                )?;
            }
            // Simulates a helper crash: the connection goes away without
            // ScanDone.
            drop(conn);
            Ok(())
        });
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        client.send(scan_request()).unwrap();
        let mut batches = 0;
        let err = loop {
            match client.recv(Some(Duration::from_secs(10))) {
                Ok(Some((_, Response::ScanBatch { .. }))) => batches += 1,
                Ok(other) => panic!("{other:?}"),
                Err(e) => break e,
            }
        };
        assert_eq!(err, IpcError::Disconnected);
        assert!(batches <= 3);
        // Every later call fails the same way.
        assert_eq!(
            client.call(Request::Ping, None).unwrap_err(),
            IpcError::Disconnected
        );
        srv.join().unwrap().unwrap();
    });
}

#[test]
fn rate_limiting_refuses_bursts() {
    let (mut server, name) = server_with(|c| {
        c.rate_limit = RateLimit {
            burst: 5,
            per_second: 1,
        };
    });
    let limited = AtomicUsize::new(0);
    std::thread::scope(|s| {
        let srv = s.spawn(|| -> Result<(), IpcError> {
            let conn = server.accept(Some(Duration::from_secs(5)))?;
            loop {
                match conn.recv_request(None) {
                    Ok(Some((id, _))) => conn.send(id, Response::Pong)?,
                    Ok(None) => {}
                    Err(IpcError::RateLimited { .. }) => {
                        limited.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Err(IpcError::Disconnected) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        });
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        let ids: Vec<u32> = (0..10)
            .map(|_| client.send(Request::Ping).unwrap())
            .collect();
        let (mut pongs, mut refused) = (0, 0);
        for _ in &ids {
            match client.recv(Some(Duration::from_secs(5))) {
                Ok(Some((_, Response::Pong))) => pongs += 1,
                Err(IpcError::RateLimited {
                    request_id,
                    retry_after,
                }) => {
                    assert!(ids.contains(&request_id));
                    assert!(retry_after > Duration::ZERO);
                    refused += 1;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!((pongs, refused), (5, 5));
        drop(client);
        srv.join().unwrap().unwrap();
    });
    assert_eq!(limited.load(std::sync::atomic::Ordering::SeqCst), 5);
}

fn malformed_input_closes_the_connection(bytes: Vec<u8>, check: impl Fn(&FrameError)) {
    let (mut server, name) = server_with(|_| {});
    std::thread::scope(|s| {
        let srv = s.spawn(|| -> Result<(), IpcError> {
            let conn = server.accept(Some(Duration::from_secs(5)))?;
            let r = conn.recv_request(Some(Duration::from_secs(5)));
            drop(conn);
            r.map(|_| ())
        });
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        client.write_raw(&bytes).unwrap();
        match srv.join().unwrap() {
            Err(IpcError::Frame(e)) => check(&e),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            client.recv(Some(Duration::from_secs(5))).unwrap_err(),
            IpcError::Disconnected
        );
    });
}

#[test]
fn malformed_frames_are_rejected() {
    malformed_input_closes_the_connection(vec![5, 0, 0, 0, 77, 1, 0, 0, 0], |e| {
        assert_eq!(*e, FrameError::UnknownKind(77));
    });
    malformed_input_closes_the_connection(vec![6, 0, 0, 0, 4, 1, 0, 0, 0, 200], |e| {
        assert!(matches!(e, FrameError::Payload(_)));
    });
    malformed_input_closes_the_connection(vec![2, 0, 0, 0, 0, 0], |e| {
        assert_eq!(*e, FrameError::TooShort(2));
    });
}

#[test]
fn oversized_frames_are_rejected_before_reading_the_body() {
    let mut bytes = (MAX_FRAME_LEN + 1).to_le_bytes().to_vec();
    bytes.extend_from_slice(&[4, 1, 0, 0, 0]);
    malformed_input_closes_the_connection(bytes, |e| {
        assert!(matches!(e, FrameError::Oversized { .. }));
    });
}

#[test]
fn handshake_frames_after_the_handshake_are_hostile() {
    let (mut server, name) = server_with(|_| {});
    std::thread::scope(|s| {
        let srv = s.spawn(|| -> Result<(), IpcError> {
            let conn = server.accept(Some(Duration::from_secs(5)))?;
            conn.recv_request(Some(Duration::from_secs(5))).map(|_| ())
        });
        let client = PipeClient::connect(&name, client_opts()).unwrap();
        let mut buf = Vec::new();
        crate::frame::encode_frame(
            &Envelope {
                request_id: 1,
                message: Message::Hello(Hello {
                    protocol: PROTOCOL_VERSION,
                    build: String::new(),
                    client_pid: 1,
                }),
            },
            &mut buf,
        )
        .unwrap();
        client.write_raw(&buf).unwrap();
        assert!(matches!(srv.join().unwrap(), Err(IpcError::Unexpected(_))));
    });
}

#[test]
fn pipe_security_descriptor_matches_the_policy() {
    let (server, name) = server_with(|_| {});
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the server handle is live; `sd` receives a LocalAlloc'd
    // descriptor freed below.
    let status = unsafe {
        GetSecurityInfo(
            server.raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    assert_eq!(status.0, 0, "GetSecurityInfo");
    let mut text = PWSTR::null();
    // SAFETY: `sd` is valid; `text` receives a LocalAlloc'd string.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            &mut text,
            None,
        )
    }
    .unwrap();
    // SAFETY: `text` is a valid NUL-terminated string from the call above.
    let sddl = unsafe { text.to_string() }.unwrap();
    // SAFETY: both were allocated by the system with LocalAlloc.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    let user = sid();
    assert!(sddl.starts_with("D:P"), "{sddl}");
    assert!(sddl.contains(&format!("(A;;0x120183;;;{user})")), "{sddl}");
    assert!(sddl.contains(";;;SY)"), "{sddl}");
    assert!(sddl.contains("(A;;RC;;;OW)"), "{sddl}");
    assert_eq!(sddl.matches("(A;").count(), 3, "no other grants: {sddl}");
    assert!(!sddl.contains(";;;WD)") && !sddl.contains(";;;AU)") && !sddl.contains(";;;BU)"));
    // NOTE: Windows reports the SACL as `S:AI(...)` (auto-inherited flag).
    let sacl = &sddl[sddl.find("S:").expect("label present")..];
    assert!(sacl.ends_with("(ML;;NW;;;ME)"), "{sddl}");
    assert_eq!(sacl.matches('(').count(), 1, "{sddl}");

    // Functional checks: GENERIC_WRITE includes FILE_CREATE_PIPE_INSTANCE,
    // which the DACL withholds.
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; a successful handle would be closed
    // by OwnedHandle.
    let r = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    };
    let err = r.map(|h| {
        // SAFETY: unexpected success; take ownership so it is closed.
        drop(unsafe { OwnedHandle::from_raw(h) })
    });
    assert_eq!(
        err.unwrap_err().code(),
        windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult()
    );
    // A second server instance (squatting attempt) cannot be created.
    let sid = sid();
    let dup = PipeServer::create(ServerConfig::new(&name, sid, self_policy()));
    assert!(matches!(dup, Err(IpcError::Win(_))));
}

#[test]
fn invalid_pipe_names_are_refused() {
    let cfg = ServerConfig::new(r"\\.\pipe\not-strata", sid(), self_policy());
    assert!(matches!(PipeServer::create(cfg), Err(IpcError::Win(_))));
}
