//! Benchmarks: `cargo bench -p strata-dupes`.
//!
//! 1. Dry-run size grouping over 1M synthetic candidates (no I/O).
//! 2. Full pipeline throughput over a generated set of large files on the
//!    test drive (`STRATA_DUPES_BENCH_DIR`, default `D:\strata-dupes-tests`;
//!    `STRATA_DUPES_BENCH_MB` total size, default 4096). The set is created
//!    in a fresh subfolder and removed afterwards. Half the files are copies,
//!    so every file reaches the full-hash phase.
//! 3. Optional, read-only: `STRATA_DUPES_BENCH_REAL=<folder>` scans an
//!    existing folder (cloud placeholders are excluded before any open).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use strata_clean::CancelToken;
use strata_core::{EntryFlags, FileRef, FileTime};
use strata_dupes::{
    Candidate, MemoryHashCache, ScanConfig, ScanOutcome, VolumeKey, find_duplicates, group_by_size,
};

fn main() {
    bench_grouping();
    bench_hashing();
    if let Some(dir) = std::env::var_os("STRATA_DUPES_BENCH_REAL") {
        bench_real(Path::new(&dir));
    }
}

/// Deterministic xorshift so runs are comparable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn bench_grouping() {
    let n = 1_000_000;
    let volume = VolumeKey::new(1, r"\\?\Volume{00000000-0000-0000-0000-000000000001}\");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let candidates: Vec<Candidate> = (0..n)
        .map(|i| {
            // Log-uniform sizes 1 KiB..16 GiB, so most are below the 1 MiB
            // minimum and collisions get rarer as sizes grow, like a real
            // volume.
            let bits = 10 + rng.next() % 24;
            let size = (1u64 << bits) + rng.next() % (1u64 << bits);
            // Every 50th file is a copy of a "popular" size.
            let size = if i % 50 == 0 { 4 << 20 } else { size };
            let flags = if i % 97 == 0 {
                EntryFlags::HARDLINK_SECONDARY
            } else {
                EntryFlags::EMPTY
            };
            Candidate {
                volume: volume.clone(),
                file_ref: FileRef::from_parts(i as u64 + 64, 1),
                path: PathBuf::from(format!(r"C:\data\d{}\f{i}.bin", i % 1000)),
                size,
                mtime: FileTime(133_000_000_000_000_000 + i as u64),
                flags,
            }
        })
        .collect();
    for run in 0..3 {
        let t = Instant::now();
        let (groups, excluded) = group_by_size(&candidates, 1 << 20);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let files: usize = groups.iter().map(|g| g.1.len()).sum();
        println!(
            "grouping run {run}: {n} candidates -> {} size groups, {files} files to measure, {} excluded, {ms:.1} ms",
            groups.len(),
            excluded.values().sum::<u64>()
        );
    }
}

/// Opens for attributes only, without recalling cloud content.
fn open_no_recall(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .access_mode(0x80 | 0x0010_0000)
        .share_mode(7)
        .custom_flags(0x0010_0000 | 0x0020_0000)
        .open(path)
}

/// The file index the walker would record for `f`.
fn strata_win_index(f: &std::fs::File) -> u64 {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is owned by `f` and outlives the call; `info` is a
    // valid out-parameter.
    unsafe { GetFileInformationByHandle(HANDLE(f.as_raw_handle()), &raw mut info) }.unwrap();
    (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
}

fn candidates_in(dir: &Path, volume: &VolumeKey) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            use std::os::windows::fs::MetadataExt;
            let attrs = m.file_attributes();
            let mut flags = EntryFlags::from_win32_attributes(attrs);
            if attrs & 0x400 != 0 {
                // Cloud, symlink, junction...: the index would set a reparse
                // kind; any non-zero kind keeps it from being opened.
                flags = flags.with_reparse(strata_core::ReparseKind::Unknown);
            }
            if attrs & strata_dupes::PLACEHOLDER_ATTRIBUTES != 0 {
                flags = flags.with_cloud(strata_core::CloudState::OnlineOnly);
            }
            if m.is_dir() {
                if flags.reparse() == strata_core::ReparseKind::None {
                    stack.push(e.path());
                }
                continue;
            }
            // Only files the gates would let through are ever opened here
            // (for their id), so placeholders are never touched.
            let gated = flags.reparse() != strata_core::ReparseKind::None
                || flags.cloud() != strata_core::CloudState::None
                || flags.contains(EntryFlags::OFFLINE)
                || m.file_size() < (1 << 20);
            let id = if gated {
                FileRef::SYNTHETIC_BIT | out.len() as u64
            } else {
                match open_no_recall(&e.path()) {
                    Ok(f) => strata_win_index(&f),
                    Err(_) => continue,
                }
            };
            out.push(Candidate {
                volume: volume.clone(),
                file_ref: FileRef(id),
                path: e.path(),
                size: m.file_size(),
                mtime: FileTime(m.last_write_time()),
                flags,
            });
        }
    }
    out
}

fn bench_hashing() {
    let base = std::env::var_os("STRATA_DUPES_BENCH_DIR")
        .map_or_else(|| PathBuf::from(r"D:\strata-dupes-tests"), PathBuf::from);
    let total_mb: u64 = std::env::var("STRATA_DUPES_BENCH_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);
    let dir = base.join(format!("bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file_mb = 256u64;
    let originals = (total_mb / file_mb / 2).max(1);
    let t = Instant::now();
    let mut rng = Rng(42);
    let mut buf = vec![0u8; 1 << 20];
    for i in 0..originals {
        let a = dir.join(format!("orig-{i}.bin"));
        let mut f = std::fs::File::create(&a).unwrap();
        for _ in 0..file_mb {
            for chunk in buf.chunks_mut(8) {
                chunk.copy_from_slice(&rng.next().to_le_bytes());
            }
            f.write_all(&buf).unwrap();
        }
        drop(f);
        std::fs::copy(&a, dir.join(format!("copy-{i}.bin"))).unwrap();
    }
    let bytes = (originals * 2 * file_mb) << 20;
    println!(
        "generated {} files, {} MiB in {:.1} s",
        originals * 2,
        bytes >> 20,
        t.elapsed().as_secs_f64()
    );
    let volume = VolumeKey::new(1, "bench");
    let candidates = candidates_in(&dir, &volume);
    for concurrency in [1, 2, 4] {
        let cfg = ScanConfig {
            concurrency,
            ..ScanConfig::default()
        };
        let cache = MemoryHashCache::new();
        let t = Instant::now();
        let out = find_duplicates(
            candidates.clone(),
            &cfg,
            &cache,
            &CancelToken::new(),
            &|_| {},
        );
        let s = t.elapsed().as_secs_f64();
        let ScanOutcome::Completed(r) = out else {
            panic!("cancelled")
        };
        println!(
            "pipeline concurrency {concurrency}: {} groups, {} MiB read in {s:.2} s = {:.0} MB/s (OS cache warm: files were just written)",
            r.groups.len(),
            r.stats.bytes_read >> 20,
            r.stats.bytes_read as f64 / 1e6 / s
        );
        // Resume path: everything comes from the cache.
        let t = Instant::now();
        let ScanOutcome::Completed(r2) = find_duplicates(
            candidates.clone(),
            &cfg,
            &cache,
            &CancelToken::new(),
            &|_| {},
        ) else {
            panic!("cancelled")
        };
        println!(
            "  cached rerun: {} groups, {} bytes read, {:.1} ms",
            r2.groups.len(),
            r2.stats.bytes_read,
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    let cfg = ScanConfig {
        concurrency: 2,
        max_bytes_per_sec: Some(200 << 20),
        ..ScanConfig::default()
    };
    let t = Instant::now();
    let _ = find_duplicates(
        candidates,
        &cfg,
        &MemoryHashCache::new(),
        &CancelToken::new(),
        &|_| {},
    );
    let s = t.elapsed().as_secs_f64();
    println!(
        "throttled to 200 MiB/s: {:.0} MiB/s observed",
        (bytes >> 20) as f64 / s
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

fn bench_real(dir: &Path) {
    let volume = VolumeKey::new(2, "real");
    let t = Instant::now();
    let candidates = candidates_in(dir, &volume);
    println!(
        "real folder: {} candidates listed in {:.1} s",
        candidates.len(),
        t.elapsed().as_secs_f64()
    );
    let t = Instant::now();
    let out = find_duplicates(
        candidates,
        &ScanConfig::default(),
        &MemoryHashCache::new(),
        &CancelToken::new(),
        &|_| {},
    );
    let s = t.elapsed().as_secs_f64();
    if let ScanOutcome::Completed(r) = out {
        let st = &r.stats;
        println!(
            "real folder: measured {}, partial {}, full {}, {} MiB read, {} groups, {} MiB wasted, {:.1} s ({:.0} MB/s)",
            st.measured,
            st.partial_hashed,
            st.full_hashed,
            st.bytes_read >> 20,
            st.groups,
            st.wasted_bytes >> 20,
            s,
            st.bytes_read as f64 / 1e6 / s
        );
    }
}
