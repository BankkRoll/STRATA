//! CPU-side scanner benchmarks.
//!
//! - `parse/1M_records_single_thread`: fixups + parse + assemble for 1M
//!   records on one core (distinct records cycled from a 64k-record pool).
//! - `parse/1M_records_rayon`: the same work spread over the rayon pool.
//! - `pipeline/in_memory_image`: `NtfsVolume::scan` end to end over an
//!   in-memory image (I/O thread + channel + rayon + sink).

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use rayon::prelude::*;
use strata_core::FileRef;
use strata_ntfs::test_image::{
    Geometry, ImageBuilder, NonResidentSpec, RecordBuilder, symlink_reparse,
};
use strata_ntfs::{NS_DOS, NS_WIN32, NtfsVolume, ParseOptions, RecordOutcome, Run, ScanOptions};
use strata_ntfs::{assemble, parse_record};

const POOL: usize = 65_536;
const MILLION: usize = 1_000_000;

/// A mix resembling a system volume: mostly small files, some non-resident,
/// some directories, a few hardlinks and reparse points.
fn sample_record(i: usize) -> RecordBuilder {
    let parent = FileRef::from_parts(100 + (i as u64 % 5000), 1);
    let name = format!("file_{i:07}.dat");
    let base = RecordBuilder::file(1, parent, &name);
    match i % 10 {
        0 => RecordBuilder::dir(1, parent, &format!("dir_{i}")),
        1 | 2 => base.data_nonresident(
            "",
            0,
            NonResidentSpec::new(
                vec![Run {
                    vcn: 0,
                    lcn: Some(1000 + i as u64),
                    len: 3,
                }],
                10_000,
                4096,
            ),
        ),
        3 => base
            .name(parent, &format!("FILE_{:03}.DAT", i % 1000), NS_DOS)
            .name(FileRef::from_parts(7000, 1), &format!("link_{i}"), NS_WIN32)
            .data("", &[0u8; 100]),
        4 => base
            .reparse(symlink_reparse(r"\??\C:\target", r"C:\target", false))
            .data("", &[]),
        5 => base
            .data("", &[1u8; 300])
            .data("Zone.Identifier", &[0u8; 26]),
        _ => base.data("", &[0u8; 40]),
    }
}

fn record_pool(g: Geometry) -> Vec<u8> {
    let rs = g.record_size as usize;
    let mut out = Vec::with_capacity(POOL * rs);
    for i in 0..POOL {
        out.extend_from_slice(&sample_record(i).build(g, 100 + i as u64));
    }
    out
}

fn parse_one(rec: &mut [u8], n: u64, opts: &ParseOptions) -> usize {
    match parse_record(rec, n, opts) {
        RecordOutcome::InUse(p) => assemble(*p, Vec::new(), None, 4096).links.len(),
        _ => 0,
    }
}

fn bench_parse(c: &mut Criterion) {
    let g = Geometry::default();
    let rs = g.record_size as usize;
    let pool = record_pool(g);
    let opts = ParseOptions::scan(1 << 40);

    let mut group = c.benchmark_group("parse");
    group.sample_size(10);
    group.throughput(Throughput::Elements(MILLION as u64));
    group.bench_function("1M_records_single_thread", |b| {
        let mut buf = vec![0u8; rs];
        b.iter(|| {
            let mut links = 0;
            for i in 0..MILLION {
                let k = i % POOL;
                buf.copy_from_slice(&pool[k * rs..(k + 1) * rs]);
                links += parse_one(&mut buf, k as u64, &opts);
            }
            black_box(links)
        });
    });
    // PERF: refilling a 64 MiB buffer per round (as the I/O thread would)
    // keeps memory flat instead of cloning a 1 GiB image per iteration. The
    // memcpy is included in the timing, which only makes the number pessimistic.
    group.bench_function("1M_records_rayon", |b| {
        let mut work = vec![0u8; POOL * rs];
        b.iter(|| {
            let mut links = 0usize;
            let mut done = 0usize;
            while done < MILLION {
                let n = POOL.min(MILLION - done);
                work[..n * rs].copy_from_slice(&pool[..n * rs]);
                links += work[..n * rs]
                    .par_chunks_mut(rs)
                    .enumerate()
                    .map(|(i, rec)| parse_one(rec, i as u64, &opts))
                    .sum::<usize>();
                done += n;
            }
            black_box(links)
        });
    });
    group.finish();
}

fn bench_pipeline(c: &mut Criterion) {
    const RECORDS: usize = 200_000;
    let mut img = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .mft_fragments(16)
        .min_records(RECORDS as u64 + 64);
    for i in 0..RECORDS {
        img.insert(64 + i as u64, sample_record(i));
    }
    let volume = NtfsVolume::open(img.finish()).expect("valid image");
    let mut group = c.benchmark_group("pipeline");
    group.sample_size(10);
    group.throughput(Throughput::Elements(RECORDS as u64));
    group.bench_function("in_memory_image_200k", |b| {
        b.iter(|| {
            let mut n = 0usize;
            let stats = volume
                .scan(&ScanOptions::default(), |batch| n += batch.len())
                .expect("scan");
            black_box((n, stats.records_emitted))
        });
    });
    group.finish();
}

criterion_group!(benches, bench_parse, bench_pipeline);
criterion_main!(benches);
