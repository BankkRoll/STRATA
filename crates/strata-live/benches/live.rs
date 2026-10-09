//! Live update benchmarks (report harness, run with `cargo bench -p strata-live`).
//!
//! - Throughput: journal records per second end to end (parse, coalesce,
//!   fetch from an in-memory source, index apply) into a large index.
//! - Latency: wall time from a record entering the journal to its tick
//!   being emitted, with the default 250 ms tick, on the real clock.
//! - Idle: journal reads (wakeups) while nothing changes for a few seconds.
//!
//! `STRATA_BENCH_ENTRIES` (default 1,000,000) and `STRATA_BENCH_CHANGES`
//! (default 100,000) size the throughput run.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strata_core::{EntryFlags, FileRef, FileTime, NameLink, ScanRecord, Sizes, Times, WideName};
use strata_index::{Index, IndexBuilder, IndexOptions};
use strata_live::{
    Fetched, Halt, JournalInfo, JournalPosition, JournalSource, LiveEvent, RecordSource,
    SourceError, SystemClock, Tailer, TailerConfig,
};
use strata_ntfs::usn::{self, FileId128, UsnChange, encode};

const ROOT: FileRef = FileRef::from_parts(5, 5);
const JOURNAL_ID: u64 = 0x5EED;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn record(id: FileRef, parent: FileRef, name: &str, size: u64, dir: bool) -> ScanRecord {
    let t = FileTime::from_unix_secs(1_700_000_000);
    ScanRecord {
        id,
        links: vec![NameLink {
            parent,
            name: WideName::from_str_lossless(name),
        }],
        attributes: if dir { 0x10 } else { 0x20 },
        flags: if dir {
            EntryFlags::DIR
        } else {
            EntryFlags::EMPTY
        },
        times: Times {
            created: t,
            modified: t,
            accessed: t,
            changed: t,
        },
        fn_created: None,
        sizes: Sizes {
            logical: size,
            allocated: size.next_multiple_of(4096),
            ..Sizes::default()
        },
        reparse: None,
        ads: vec![],
    }
}

fn change(file: FileRef, parent: FileRef, name: &str, reason: u32, usn: i64) -> Vec<u8> {
    encode::change(&UsnChange {
        major_version: 2,
        file: FileId128(u128::from(file.0)),
        parent: FileId128(u128::from(parent.0)),
        usn,
        timestamp: FileTime::from_unix_secs(1_700_000_000),
        reason,
        source_info: 0,
        security_id: 0,
        attributes: 0,
        name: WideName::from_str_lossless(name),
    })
}

// -----------------------------------------------------------------------------
// Throughput
// -----------------------------------------------------------------------------

/// Journal under construction; each record gets the USN of its offset.
#[derive(Default)]
struct Log {
    records: Vec<(i64, Vec<u8>)>,
    next: i64,
}

impl Log {
    fn add(&mut self, file: FileRef, parent: FileRef, name: &str, reason: u32) {
        let bytes = change(file, parent, name, reason, self.next);
        let len = bytes.len() as i64;
        self.records.push((self.next, bytes));
        self.next += len;
    }
}

struct MemJournal {
    records: Vec<(i64, Vec<u8>)>,
    next: i64,
}

impl JournalSource for MemJournal {
    fn query(&mut self) -> Result<Option<JournalInfo>, SourceError> {
        Ok(Some(JournalInfo {
            journal_id: JOURNAL_ID,
            first_usn: 0,
            next_usn: self.next,
            lowest_valid_usn: 0,
            max_usn: i64::MAX,
        }))
    }

    fn read(&mut self, _: u64, from: i64, _: Option<Duration>) -> Result<Vec<u8>, SourceError> {
        // 64 KiB, the size the helper requests per read.
        let start = self.records.partition_point(|(u, _)| *u < from);
        let mut out = Vec::new();
        let mut bytes = 0;
        let mut i = start;
        while i < self.records.len() && bytes + self.records[i].1.len() <= 65_536 {
            bytes += self.records[i].1.len();
            out.push(self.records[i].1.clone());
            i += 1;
        }
        let next = self.records.get(i).map_or(self.next, |(u, _)| *u);
        Ok(encode::buffer(next, &out))
    }
}

struct MemRecords(HashMap<u64, ScanRecord>);

impl RecordSource for MemRecords {
    fn fetch(&mut self, refs: &[FileRef]) -> Result<Fetched, SourceError> {
        let mut out = Fetched::default();
        for &r in refs {
            match self.0.get(&r.record()).filter(|x| x.id == r) {
                Some(x) => out.records.push(x.clone()),
                None => out.missing.push(r),
            }
        }
        Ok(out)
    }
}

fn throughput() {
    let entries = env("STRATA_BENCH_ENTRIES", 1_000_000);
    let changes = env("STRATA_BENCH_CHANGES", 100_000);
    let dirs = (entries / 100).max(1);
    let mut state: HashMap<u64, ScanRecord> = HashMap::with_capacity(entries + changes);
    state.insert(5, record(ROOT, ROOT, "", 0, true));
    let dir_ref = |i: usize| FileRef::from_parts(100 + i as u64, 1);
    for i in 0..dirs {
        state.insert(
            100 + i as u64,
            record(dir_ref(i), ROOT, &format!("d{i}"), 0, true),
        );
    }
    let first_file = 100 + dirs as u64;
    for i in 0..entries.saturating_sub(dirs) {
        let rec = first_file + i as u64;
        let id = FileRef::from_parts(rec, 1);
        state.insert(
            rec,
            record(
                id,
                dir_ref(i % dirs),
                &format!("f{i}.bin"),
                (i as u64 * 7919) % 10_000_000,
                false,
            ),
        );
    }
    let t = Instant::now();
    let mut b = IndexBuilder::new(IndexOptions::default());
    b.push_batch(state.values().cloned()).expect("push");
    let mut index = b.finish().expect("finish");
    let build = t.elapsed();

    // 60% resize, 20% create, 10% move, 10% delete.
    let mut log = Log::default();
    let mut next_new = first_file + entries as u64;
    let files = entries.saturating_sub(dirs) as u64;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..changes {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let rec = first_file + x % files;
        let Some(cur) = state.get(&rec).cloned() else {
            continue;
        };
        let id = cur.id;
        let parent = cur.links[0].parent;
        let name = cur.links[0].name.to_string_lossy();
        match i % 10 {
            0..=5 => {
                let mut r = cur;
                r.sizes.logical += 4096;
                r.sizes.allocated += 4096;
                state.insert(rec, r);
                log.add(id, parent, &name, usn::USN_REASON_DATA_EXTEND);
                log.add(
                    id,
                    parent,
                    &name,
                    usn::USN_REASON_DATA_EXTEND | usn::USN_REASON_CLOSE,
                );
            }
            6 | 7 => {
                let id = FileRef::from_parts(next_new, 1);
                let p = dir_ref((x as usize >> 3) % dirs);
                let nm = format!("n{next_new}.tmp");
                state.insert(next_new, record(id, p, &nm, 1234, false));
                next_new += 1;
                log.add(id, p, &nm, usn::USN_REASON_FILE_CREATE);
                log.add(
                    id,
                    p,
                    &nm,
                    usn::USN_REASON_FILE_CREATE | usn::USN_REASON_CLOSE,
                );
            }
            8 => {
                let to = dir_ref((x as usize >> 5) % dirs);
                let nm = format!("m{i}");
                let mut r = cur;
                r.links[0] = NameLink {
                    parent: to,
                    name: WideName::from_str_lossless(&nm),
                };
                state.insert(rec, r);
                log.add(id, parent, &name, usn::USN_REASON_RENAME_OLD_NAME);
                log.add(id, to, &nm, usn::USN_REASON_RENAME_NEW_NAME);
                log.add(
                    id,
                    to,
                    &nm,
                    usn::USN_REASON_RENAME_NEW_NAME | usn::USN_REASON_CLOSE,
                );
            }
            _ => {
                state.remove(&rec);
                log.add(
                    id,
                    parent,
                    &name,
                    usn::USN_REASON_FILE_DELETE | usn::USN_REASON_CLOSE,
                );
            }
        }
    }
    let Log {
        records: journal,
        next: usn,
    } = log;
    let records_total = journal.len();
    let mut j = MemJournal {
        records: journal,
        next: usn,
    };
    let mut src = MemRecords(state);
    let cfg = TailerConfig {
        tick: Duration::ZERO,
        ..TailerConfig::default()
    };
    let mut tailer = Tailer::start(
        cfg,
        &mut j,
        JournalPosition {
            journal_id: JOURNAL_ID,
            usn: 0,
        },
    )
    .expect("start");
    let mut ticks = 0;
    let mut longest = Duration::ZERO;
    let t = Instant::now();
    while tailer.read_position() < usn || tailer.pending() > 0 {
        tailer
            .step(&mut j, &mut src, &mut index, &SystemClock, &mut |e| {
                if let LiveEvent::Tick(r) = e {
                    ticks += 1;
                    longest = longest.max(r.elapsed);
                }
            })
            .expect("step");
    }
    let took = t.elapsed();
    let s = tailer.stats();
    println!(
        "throughput: index {entries} entries (built in {build:.2?}); {records_total} journal records \
         ({changes} changes) applied in {took:.2?} = {:.0} records/s, {:.0} refreshed refs/s; \
         {} fetched, {} updates, {ticks} ticks, slowest tick {longest:.2?}",
        records_total as f64 / took.as_secs_f64(),
        s.updates as f64 / took.as_secs_f64(),
        s.fetched,
        s.updates
    );
    index.check_invariants().expect("invariants");
}

// -----------------------------------------------------------------------------
// Latency and idle (real clock, blocking source)
// -----------------------------------------------------------------------------

/// Blocks on a channel like `FSCTL_READ_USN_JOURNAL` blocks on the volume.
struct ChannelJournal {
    rx: Receiver<Vec<u8>>,
    next: i64,
    reads: Arc<AtomicU64>,
}

impl JournalSource for ChannelJournal {
    fn query(&mut self) -> Result<Option<JournalInfo>, SourceError> {
        Ok(Some(JournalInfo {
            journal_id: JOURNAL_ID,
            first_usn: 0,
            next_usn: self.next,
            lowest_valid_usn: 0,
            max_usn: i64::MAX,
        }))
    }

    fn read(&mut self, _: u64, from: i64, wait: Option<Duration>) -> Result<Vec<u8>, SourceError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let got = match wait {
            None => self.rx.recv().map_err(|_| SourceError::Disconnected),
            Some(w) => match self.rx.recv_timeout(w) {
                Ok(b) => Ok(b),
                Err(RecvTimeoutError::Timeout) => return Ok(from.to_le_bytes().to_vec()),
                Err(RecvTimeoutError::Disconnected) => Err(SourceError::Disconnected),
            },
        }?;
        Ok(got)
    }
}

struct SharedRecords(Arc<Mutex<HashMap<u64, ScanRecord>>>);

impl RecordSource for SharedRecords {
    fn fetch(&mut self, refs: &[FileRef]) -> Result<Fetched, SourceError> {
        MemRecords(self.0.lock().expect("records").clone()).fetch(refs)
    }
}

fn small_index() -> Index {
    let mut b = IndexBuilder::new(IndexOptions::default());
    b.push(record(ROOT, ROOT, "", 0, true)).expect("root");
    b.finish().expect("finish")
}

fn latency_and_idle() {
    const SAMPLES: usize = 12;
    let (tx, rx): (Sender<Vec<u8>>, _) = channel();
    let reads = Arc::new(AtomicU64::new(0));
    let state = Arc::new(Mutex::new(HashMap::from([(
        5u64,
        record(ROOT, ROOT, "", 0, true),
    )])));
    let sent: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let idle_reads = Arc::new(AtomicU64::new(0));

    let producer = {
        let state = state.clone();
        let sent = sent.clone();
        let reads = reads.clone();
        let idle_reads = idle_reads.clone();
        std::thread::spawn(move || {
            let mut usn = 0i64;
            // Idle phase first: nothing changes for three seconds.
            std::thread::sleep(Duration::from_millis(200));
            let before = reads.load(Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(3));
            idle_reads.store(reads.load(Ordering::Relaxed) - before, Ordering::Relaxed);
            for i in 0..SAMPLES {
                let rec = 1000 + i as u64;
                let id = FileRef::from_parts(rec, 1);
                let name = format!("live{i}.txt");
                state
                    .lock()
                    .expect("state")
                    .insert(rec, record(id, ROOT, &name, 100, false));
                let a = change(id, ROOT, &name, usn::USN_REASON_FILE_CREATE, usn);
                let b = change(
                    id,
                    ROOT,
                    &name,
                    usn::USN_REASON_FILE_CREATE | usn::USN_REASON_CLOSE,
                    usn + a.len() as i64,
                );
                let next = usn + (a.len() + b.len()) as i64;
                sent.lock().expect("sent").push(Instant::now());
                tx.send(encode::buffer(next, &[a, b])).expect("send");
                usn = next;
                std::thread::sleep(Duration::from_millis(450 + (i as u64 * 37) % 200));
            }
            std::thread::sleep(Duration::from_millis(400));
            // Dropping the sender disconnects the source and ends the run.
        })
    };

    let mut journal = ChannelJournal {
        rx,
        next: 0,
        reads: reads.clone(),
    };
    let mut records = SharedRecords(state);
    let mut index = small_index();
    let mut tailer = Tailer::start(
        TailerConfig::default(),
        &mut journal,
        JournalPosition {
            journal_id: JOURNAL_ID,
            usn: 0,
        },
    )
    .expect("start");
    let mut emitted = Vec::new();
    let stop = AtomicBool::new(false);
    let halt = tailer.run(
        &mut journal,
        &mut records,
        &mut index,
        &SystemClock,
        &stop,
        &mut |e| {
            if let LiveEvent::Tick(_) = e {
                emitted.push(Instant::now());
            }
        },
    );
    producer.join().expect("producer");
    assert!(matches!(halt, Halt::Stale(_)));

    let sent = sent.lock().expect("sent");
    let mut lat: Vec<Duration> = sent.iter().zip(&emitted).map(|(s, e)| *e - *s).collect();
    lat.sort();
    println!(
        "latency (tick 250 ms, {} samples): min {:.1?}, median {:.1?}, max {:.1?}",
        lat.len(),
        lat.first().copied().unwrap_or_default(),
        lat.get(lat.len() / 2).copied().unwrap_or_default(),
        lat.last().copied().unwrap_or_default()
    );
    println!(
        "idle: {} journal reads (wakeups) in 3 s with nothing changing; total reads {}",
        idle_reads.load(Ordering::Relaxed),
        reads.load(Ordering::Relaxed)
    );
}

fn main() {
    // `cargo test --benches` runs bench targets with `--bench` absent; keep
    // that fast by only reporting when invoked through `cargo bench`.
    if !std::env::args().any(|a| a == "--bench") {
        return;
    }
    throughput();
    latency_and_idle();
}
