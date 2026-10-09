//! Index benchmarks (SPEC §2, §22).
//!
//! Running `cargo bench -p strata-index` first prints a one-shot report of
//! the numbers that are not plain timings (bytes per entry, first-batch
//! search latency, update throughput, cache size) on 1M- and 5M-entry
//! synthetic volumes, then runs the criterion timings.
//!
//! `STRATA_BENCH_MAX=1000000` skips the 5M report; `STRATA_BENCH_REPORT=0`
//! skips the report entirely; `STRATA_BENCH_CRITERION=0` runs only the report.

#[path = "support/synth.rs"]
mod synth;

use std::hint::black_box;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion};
use memchr::memmem;
use strata_core::{EntryFlags, FileRef, FileTime, NameLink, ScanRecord, SizeMode, Sizes, WideName};
use strata_index::search::{CancelToken, Query, SearchOptions};
use strata_index::{
    BuildStats, ChildQuery, EntryId, EntryKind, Index, IndexBuilder, IndexOptions, SortKey, Update,
};

fn opts(lite: bool) -> IndexOptions {
    IndexOptions {
        lite,
        now: synth::now(),
        volume: strata_index::VolumeInfo {
            prefix: "C:".into(),
            ..Default::default()
        },
        ..IndexOptions::default()
    }
}

/// Streams a synthetic volume into a builder: (index, staging time, stats).
fn build_streamed(n: usize, lite: bool) -> (Index, Duration, BuildStats) {
    let mut b = IndexBuilder::new(opts(lite));
    b.reserve(n);
    let mut staging = Duration::ZERO;
    let mut batch = Vec::with_capacity(4096);
    synth::generate(n, 42, &mut |r| {
        batch.push(r);
        if batch.len() == 4096 {
            let t = Instant::now();
            b.push_batch(batch.drain(..)).unwrap();
            staging += t.elapsed();
        }
    });
    let t = Instant::now();
    b.push_batch(batch).unwrap();
    staging += t.elapsed();
    let (index, stats) = b.finish_with_stats().unwrap();
    (index, staging, stats)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

// -----------------------------------------------------------------------------
// One-shot report
// -----------------------------------------------------------------------------

fn report() {
    let max: usize = std::env::var("STRATA_BENCH_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000_000);
    println!(
        "# strata-index report ({} threads)\n",
        rayon::current_num_threads()
    );
    println!(
        "| entries | mode | staging ms | finish ms | resolve | cycles | layout | aggregate | B/entry (excl. names) | name B/entry | dirs % |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    let mut big = None;
    for (n, lite) in [(1_000_000, false), (1_000_000, true), (5_000_000, false)] {
        if n > max {
            continue;
        }
        let (idx, staging, s) = build_streamed(n, lite);
        let m = idx.memory_report();
        println!(
            "| {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.2} | {:.2} | {:.1} |",
            idx.len(),
            if lite { "lite" } else { "full" },
            ms(staging),
            ms(s.total),
            ms(s.resolve),
            ms(s.cycles),
            ms(s.layout),
            ms(s.aggregate),
            m.bytes_per_entry(),
            m.name_bytes as f64 / m.live as f64,
            100.0 * m.dir_rows as f64 / m.live as f64,
        );
        println!("<!-- {m:?} -->");
        if n == 1_000_000 && !lite {
            encoding_report(&idx);
            live_report(n);
            cache_report(&idx);
        }
        if n == max.min(5_000_000) && !lite {
            big = Some(idx);
        }
    }
    if let Some(idx) = big {
        search_report(&idx);
        query_report(&idx);
    }
    children_report();
    println!();
}

/// WTF-8 + memmem vs UTF-16 + naive window search, single-threaded, same
/// per-name folding work, plus buffer sizes.
fn encoding_report(idx: &Index) {
    let ids: Vec<EntryId> = (0..idx.slot_count() as u32)
        .map(EntryId)
        .filter(|&e| idx.is_live(e))
        .collect();
    let utf16: Vec<Vec<u16>> = ids.iter().map(|&e| idx.name(e).units().to_vec()).collect();
    let wtf8: Vec<Vec<u8>> = ids.iter().map(|&e| idx.name_wtf8(e).to_vec()).collect();
    let b16: usize = utf16.iter().map(|v| v.len() * 2).sum();
    let b8: usize = wtf8.iter().map(Vec::len).sum();
    let needle = "react";
    let n16: Vec<u16> = needle.encode_utf16().collect();
    let finder = memmem::Finder::new(needle.as_bytes());

    let t = Instant::now();
    let mut buf16 = Vec::new();
    let mut hits16 = 0;
    for name in &utf16 {
        buf16.clear();
        buf16.extend(
            name.iter()
                .map(|&u| if (65..=90).contains(&u) { u + 32 } else { u }),
        );
        if buf16.windows(n16.len()).any(|w| w == n16.as_slice()) {
            hits16 += 1;
        }
    }
    let d16 = t.elapsed();
    let t = Instant::now();
    let mut buf8 = Vec::new();
    let mut hits8 = 0;
    for name in &wtf8 {
        buf8.clear();
        buf8.extend(name.iter().map(u8::to_ascii_lowercase));
        if finder.find(&buf8).is_some() {
            hits8 += 1;
        }
    }
    let d8 = t.elapsed();
    assert_eq!(hits8, hits16);
    println!(
        "\nName encoding on {} names: UTF-16 {:.1} MB, scan {:.1} ms; WTF-8 {:.1} MB, scan {:.1} ms (single thread, needle \"{needle}\", {hits8} hits)\n",
        ids.len(),
        b16 as f64 / 1e6,
        ms(d16),
        b8 as f64 / 1e6,
        ms(d8)
    );
}

fn search_report(idx: &Index) {
    println!("\nSearch over {} entries (median of 7):\n", idx.len());
    println!("| query | first batch ms | complete ms | matches |");
    println!("|---|---|---|---|");
    for q in [
        "react",
        "^index",
        "*.gguf",
        "*.mp4",
        "IMG_0*.jpg",
        r"node_modules\react*",
        "size:>100mb",
        "ext:dll size:>1mb",
        r"re:^DSC\d{5}\.NEF$",
        "zzzz-no-match",
    ] {
        let query = Query::parse(q, synth::now()).unwrap();
        let mut firsts = Vec::new();
        let mut totals = Vec::new();
        let mut matched = 0;
        for _ in 0..7 {
            let first = Mutex::new(None::<Duration>);
            let t = Instant::now();
            let out = idx
                .search(
                    &query,
                    &SearchOptions::default(),
                    &CancelToken::new(),
                    &|_| {
                        let mut f = first.lock().unwrap();
                        if f.is_none() {
                            *f = Some(t.elapsed());
                        }
                    },
                )
                .unwrap();
            let total = t.elapsed();
            totals.push(total);
            firsts.push(first.into_inner().unwrap().unwrap_or(total));
            matched = out.matched;
        }
        println!(
            "| `{q}` | {:.2} | {:.2} | {matched} |",
            ms(median(firsts)),
            ms(median(totals))
        );
    }
}

fn query_report(idx: &Index) {
    let t = Instant::now();
    let top = idx.top_n(None, 100, EntryKind::Files, SizeMode::Allocated, None);
    let d_top = t.elapsed();
    let t = Instant::now();
    let b = idx.extension_breakdown(None, SizeMode::Allocated);
    let d_ext = t.elapsed();
    let t = Instant::now();
    for (id, _) in &top {
        black_box(idx.path_string(*id));
    }
    let mut cold = 0;
    for i in (0..idx.slot_count() as u32).step_by(50) {
        if idx.is_live(EntryId(i)) {
            black_box(idx.path_string(EntryId(i)));
            cold += 1;
        }
    }
    let d_path = t.elapsed();
    println!(
        "\nOn {} entries: top-100 files {:.1} ms; extension breakdown ({} exts) {:.1} ms; {} paths {:.1} ms ({:.2} µs each)",
        idx.len(),
        ms(d_top),
        b.len(),
        ms(d_ext),
        cold + top.len(),
        ms(d_path),
        d_path.as_secs_f64() * 1e6 / (cold + top.len()) as f64
    );
}

fn cache_report(idx: &Index) {
    let t = Instant::now();
    let bytes = idx.to_bytes();
    let d_save = t.elapsed();
    let t = Instant::now();
    let back = Index::from_bytes(&bytes).unwrap();
    let d_load = t.elapsed();
    assert_eq!(back.len(), idx.len());
    println!(
        "Cache on {} entries: {:.1} MB, serialize {:.0} ms, load + validate {:.0} ms\n",
        idx.len(),
        bytes.len() as f64 / 1e6,
        ms(d_save),
        ms(d_load)
    );
}

/// Mixed live updates on a 1M index: 60% resize, 20% create, 10% rename,
/// 10% delete.
fn live_report(n: usize) {
    let recs = synth::records(n, 42);
    let mut b = IndexBuilder::new(opts(false));
    b.push_batch(recs.iter().cloned()).unwrap();
    let mut idx = b.finish().unwrap();
    let files: Vec<&ScanRecord> = recs.iter().filter(|r| !r.is_dir()).collect();
    let dirs: Vec<FileRef> = recs.iter().filter(|r| r.is_dir()).map(|r| r.id).collect();
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let updates: Vec<Update> = (0..100_000u64)
        .map(|i| {
            let f = files[(next() % files.len() as u64) as usize];
            match next() % 10 {
                0..=5 => {
                    let mut r = f.clone();
                    r.sizes.logical = next() % (1 << 30);
                    r.sizes.allocated = r.sizes.logical.next_multiple_of(4096);
                    Update::Upsert(r)
                }
                6 | 7 => {
                    let parent = dirs[(next() % dirs.len() as u64) as usize];
                    Update::Upsert(ScanRecord {
                        id: FileRef::from_parts(50_000_000 + i, 1),
                        links: vec![NameLink {
                            parent,
                            name: WideName::from_str_lossless(&format!("new-{i}.tmp")),
                        }],
                        attributes: 0,
                        flags: EntryFlags::EMPTY,
                        times: f.times,
                        fn_created: None,
                        sizes: Sizes {
                            logical: 1234,
                            allocated: 4096,
                            ..Sizes::default()
                        },
                        reparse: None,
                        ads: vec![],
                    })
                }
                8 => {
                    let mut r = f.clone();
                    if let Some(l) = r.links.first_mut() {
                        l.name = WideName::from_str_lossless(&format!("renamed-{i}"));
                    }
                    Update::Upsert(r)
                }
                _ => Update::Remove(f.id),
            }
        })
        .collect();
    let total = updates.len();
    let t = Instant::now();
    let mut changed_dirs = 0;
    for chunk in updates.chunks(1000) {
        let cs = idx.apply(chunk.iter().cloned()).unwrap();
        changed_dirs += cs.aggregates.len();
    }
    let d = t.elapsed();
    idx.check_invariants().unwrap();
    println!(
        "Live updates on {} entries: {total} mixed updates in {:.0} ms = {:.0} updates/s ({changed_dirs} directory aggregates reported in 1000-update batches)",
        idx.len(),
        ms(d),
        total as f64 / d.as_secs_f64()
    );
}

fn big_dir_index() -> Index {
    let root = synth::ROOT;
    let dir = FileRef::from_parts(64, 1);
    let mut b = IndexBuilder::new(opts(false));
    let mut recs = synth::records(1, 1);
    recs.push(ScanRecord {
        id: dir,
        links: vec![NameLink {
            parent: root,
            name: WideName::from_str_lossless("Downloads"),
        }],
        attributes: 0x10,
        flags: EntryFlags::DIR,
        times: Default::default(),
        fn_created: None,
        sizes: Sizes::default(),
        reparse: None,
        ads: vec![],
    });
    let mut rng = 7u64;
    for i in 0..100_000u64 {
        rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        recs.push(ScanRecord {
            id: FileRef::from_parts(100 + i, 1),
            links: vec![NameLink {
                parent: dir,
                name: WideName::from_str_lossless(&format!("file {} ({i}).bin", rng % 1000)),
            }],
            attributes: 0,
            flags: EntryFlags::EMPTY,
            times: Default::default(),
            fn_created: None,
            sizes: Sizes {
                logical: rng >> 40,
                allocated: (rng >> 40).next_multiple_of(4096),
                ..Sizes::default()
            },
            reparse: None,
            ads: vec![],
        });
    }
    b.push_batch(recs).unwrap();
    b.finish().unwrap()
}

fn children_report() {
    let idx = big_dir_index();
    let dir = idx.lookup(FileRef::from_parts(64, 1)).unwrap();
    for (label, key, limit) in [
        ("size, full", SortKey::Size, usize::MAX),
        ("size, first page of 100", SortKey::Size, 100),
        ("name, full", SortKey::Name, usize::MAX),
        ("name, first page of 100", SortKey::Name, 100),
    ] {
        let q = ChildQuery {
            key,
            descending: key == SortKey::Size,
            limit,
            ..ChildQuery::default()
        };
        let times: Vec<Duration> = (0..5)
            .map(|_| {
                let t = Instant::now();
                black_box(idx.children_sorted(dir, &q));
                t.elapsed()
            })
            .collect();
        println!(
            "Children of a 100k-entry directory sorted by {label}: {:.1} ms",
            ms(median(times))
        );
    }
}

// -----------------------------------------------------------------------------
// Criterion
// -----------------------------------------------------------------------------

fn benches(c: &mut Criterion) {
    let recs = synth::records(1_000_000, 42);
    let mut g = c.benchmark_group("build");
    g.sample_size(10);
    g.bench_function("1m_records", |b| {
        b.iter_batched(
            || recs.clone(),
            |r| {
                let mut builder = IndexBuilder::new(opts(false));
                builder.push_batch(r).unwrap();
                builder.finish().unwrap()
            },
            BatchSize::PerIteration,
        );
    });
    g.finish();

    let mut builder = IndexBuilder::new(opts(false));
    builder.push_batch(recs.iter().cloned()).unwrap();
    let mut idx = builder.finish().unwrap();

    let mut g = c.benchmark_group("query_1m");
    g.bench_function("top_100_files", |b| {
        b.iter(|| idx.top_n(None, 100, EntryKind::Files, SizeMode::Allocated, None));
    });
    g.bench_function("extension_breakdown", |b| {
        b.iter(|| idx.extension_breakdown(None, SizeMode::Allocated));
    });
    g.finish();

    let mut g = c.benchmark_group("search_1m");
    for q in [
        "react",
        "*.gguf",
        r"node_modules\react*",
        "size:>100mb",
        r"re:^DSC\d{5}\.NEF$",
    ] {
        let query = Query::parse(q, FileTime(0)).unwrap();
        g.bench_function(q, |b| {
            b.iter(|| {
                idx.search(
                    &query,
                    &SearchOptions::default(),
                    &CancelToken::new(),
                    &|_| {},
                )
                .unwrap()
            });
        });
    }
    g.finish();

    let big = big_dir_index();
    let dir = big.lookup(FileRef::from_parts(64, 1)).unwrap();
    let mut g = c.benchmark_group("children_100k");
    for (label, key) in [("by_size", SortKey::Size), ("by_name", SortKey::Name)] {
        let q = ChildQuery {
            key,
            limit: 200,
            ..ChildQuery::default()
        };
        g.bench_function(label, |b| b.iter(|| big.children_sorted(dir, &q)));
    }
    g.finish();

    let files: Vec<ScanRecord> = recs
        .iter()
        .filter(|r| !r.is_dir())
        .step_by(997)
        .take(1000)
        .cloned()
        .collect();
    let mut flip = false;
    let mut g = c.benchmark_group("live_1m");
    g.bench_function("resize_batch_1000", |b| {
        b.iter(|| {
            flip = !flip;
            let batch = files.iter().map(|r| {
                let mut r = r.clone();
                if flip {
                    r.sizes.allocated += 4096;
                }
                Update::Upsert(r)
            });
            idx.apply(batch).unwrap()
        });
    });
    g.finish();
}

fn main() {
    if std::env::var("STRATA_BENCH_REPORT").as_deref() != Ok("0") {
        report();
    }
    if std::env::var("STRATA_BENCH_CRITERION").as_deref() == Ok("0") {
        return;
    }
    let mut c = Criterion::default().configure_from_args();
    benches(&mut c);
    c.final_summary();
}
