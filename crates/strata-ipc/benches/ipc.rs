//! Codec and pipe benchmarks: `cargo bench -p strata-ipc`.
//!
//! 1. Encode + decode of 100k `ScanRecord`s with postcard (the wire format)
//!    and bincode 2 (the alternative considered).
//! 2. Streaming 1M records over a real named pipe in 8192-record batches.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use strata_core::{EntryFlags, FileRef, FileTime, NameLink, ScanRecord, Sizes, Times, WideName};
use strata_ipc::pipe::{ClientOptions, PipeClient, PipeServer, ServerConfig};
use strata_ipc::protocol::{Request, Response, ScanOptions};
use strata_ipc::security::{PeerIdentity, PeerVerifier, TrustError, session_pipe_name};

const RECORDS: u64 = 100_000;
const ROUNDS: u32 = 10;

fn record(i: u64) -> ScanRecord {
    ScanRecord {
        id: FileRef::from_parts(i + 16, (i % 7) as u16 + 1),
        links: vec![NameLink {
            parent: FileRef::from_parts(5 + i / 64, 1),
            name: WideName::from_str_lossless(&format!("file-{i:08}.bin")),
        }],
        attributes: 0x20,
        flags: EntryFlags::EMPTY,
        times: Times {
            created: FileTime(133_000_000_000_000_000 + i * 13),
            modified: FileTime(133_000_000_000_000_000 + i * 17),
            accessed: FileTime(133_000_000_000_000_000 + i * 19),
            changed: FileTime(133_000_000_000_000_000 + i * 23),
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

fn report(name: &str, bytes: usize, encode: Duration, decode: Duration) {
    let per = |d: Duration| d.as_nanos() as f64 / f64::from(ROUNDS) / RECORDS as f64;
    let mbps = |d: Duration| bytes as f64 * f64::from(ROUNDS) / d.as_secs_f64() / 1e6;
    println!(
        "{name:9} {:>9} bytes ({:.1} B/record)  encode {:.1} ns/record ({:.0} MB/s)  decode {:.1} ns/record ({:.0} MB/s)",
        bytes,
        bytes as f64 / RECORDS as f64,
        per(encode),
        mbps(encode),
        per(decode),
        mbps(decode),
    );
}

fn bench_codecs() {
    let records: Vec<ScanRecord> = (0..RECORDS).map(record).collect();

    let mut bytes = Vec::new();
    let t = Instant::now();
    for _ in 0..ROUNDS {
        bytes = postcard::to_allocvec(black_box(&records)).unwrap();
    }
    let enc = t.elapsed();
    let t = Instant::now();
    for _ in 0..ROUNDS {
        let v: Vec<ScanRecord> = postcard::from_bytes(black_box(&bytes)).unwrap();
        black_box(v);
    }
    report("postcard", bytes.len(), enc, t.elapsed());

    let cfg = bincode::config::standard();
    let t = Instant::now();
    for _ in 0..ROUNDS {
        bytes = bincode::serde::encode_to_vec(black_box(&records), cfg).unwrap();
    }
    let enc = t.elapsed();
    let t = Instant::now();
    for _ in 0..ROUNDS {
        let (v, _): (Vec<ScanRecord>, usize) =
            bincode::serde::decode_from_slice(black_box(&bytes), cfg).unwrap();
        black_box(v);
    }
    report("bincode2", bytes.len(), enc, t.elapsed());
}

/// Accepts only this very process (both ends live in the benchmark).
#[derive(Debug)]
struct SelfOnly;

impl PeerVerifier for SelfOnly {
    fn verify(&self, peer: &PeerIdentity) -> Result<(), TrustError> {
        if peer.pid == std::process::id() {
            Ok(())
        } else {
            Err(TrustError::ImageMismatch {
                expected: std::env::current_exe().unwrap_or_default(),
                actual: peer.image.clone(),
            })
        }
    }
}

fn bench_pipe() {
    const TOTAL: u64 = 1_000_000;
    const BATCH: u64 = 8192;
    let sid = strata_win::process::current_user().unwrap().sid;
    let name = session_pipe_name(&sid).unwrap();
    let mut server = PipeServer::create(ServerConfig::new(&name, sid, Arc::new(SelfOnly))).unwrap();
    let batches: Vec<Vec<ScanRecord>> = (0..TOTAL.div_ceil(BATCH))
        .map(|b| {
            (b * BATCH..((b + 1) * BATCH).min(TOTAL))
                .map(record)
                .collect()
        })
        .collect();
    let bytes: usize = batches
        .iter()
        .map(|b| postcard::to_allocvec(b).unwrap().len() + 10)
        .sum();
    std::thread::scope(|s| {
        s.spawn(|| {
            let conn = server.accept(Some(Duration::from_secs(10))).unwrap();
            let (id, _) = conn.recv_request(None).unwrap().unwrap();
            for b in &batches {
                conn.send(id, Response::ScanBatch { records: b.clone() })
                    .unwrap();
            }
            conn.send(id, Response::ShuttingDown).unwrap();
            conn.close(Duration::from_secs(10));
        });
        let client = PipeClient::connect(&name, ClientOptions::default()).unwrap();
        let start = Instant::now();
        client
            .send(Request::ScanVolume {
                volume: "bench".into(),
                options: ScanOptions::default(),
            })
            .unwrap();
        let mut got = 0u64;
        loop {
            match client.recv(None).unwrap().unwrap().1 {
                Response::ScanBatch { records } => got += records.len() as u64,
                Response::ShuttingDown => break,
                other => panic!("{other:?}"),
            }
        }
        let secs = start.elapsed().as_secs_f64();
        assert_eq!(got, TOTAL);
        println!(
            "pipe      {TOTAL} records in {secs:.3}s: {:.2} M records/s, {:.0} MB/s (includes clone + encode + decode)",
            TOTAL as f64 / secs / 1e6,
            bytes as f64 / secs / 1e6,
        );
    });
}

fn main() {
    if std::env::args().any(|a| a == "--list") {
        return;
    }
    bench_codecs();
    bench_pipe();
}
