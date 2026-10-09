//! End-to-end timings of the app backend, outside the UI.
//!
//! ```text
//! cargo run -p strata-app --example pipeline_bench --release -- <folder>
//! cargo run -p strata-app --example pipeline_bench --release -- --synthetic 1000000
//! ```
//!
//! With a folder: walks it with the default walker options (read-only),
//! reporting time to the first preview and to its first layout frame
//! (scan-to-first-paint on the backend side), scan and finish times, index
//! memory, and layout request → frame-encoded latencies for the root and
//! for the directory closest to 100k entries. With `--synthetic N`: builds
//! an N-entry tree in memory and reports the same layout numbers.
//!
//! Prints counts and timings only, never paths.

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use strata_app_lib::classify::{Engine, classify_index, ext_slots};
use strata_app_lib::layout_pipe::{LayoutRequest, LayoutStream, Style, TransformDto, View};
use strata_app_lib::model::{ScannerUsed, ViewFilters, VolumeData};
use strata_app_lib::scan::{
    DataSlot, Ingest, ScanObserver, ScanProgress, ScanTarget, ShadowBytes, walk,
};
use strata_core::{
    EntryFlags, FileRef, FileTime, NameLink, ScanRecord, SizeMode, Sizes, Times, WideName,
};
use strata_index::{EntryId, IndexBuilder, IndexOptions};

/// Records when the first preview appeared and when its first treemap
/// frame was ready, laying it out the way the UI would on that event.
struct FirstPreview<'a> {
    started: Instant,
    slot: &'a DataSlot,
    at: Mutex<Option<(Duration, Duration, usize)>>,
}

impl ScanObserver for FirstPreview<'_> {
    fn progress(&self, _: &ScanProgress) {}
    fn preview(&self) {
        let mut g = self.at.lock().unwrap();
        if g.is_none() {
            let shown = self.started.elapsed();
            let data = self.slot.read().unwrap();
            if let Some(d) = data.as_ref() {
                time_layout(d, d.root_wire(), View::Treemap, 1);
                *g = Some((shown, self.started.elapsed(), d.index.len()));
            }
        }
    }
}

fn request(root: u32, view: View) -> LayoutRequest {
    LayoutRequest {
        volume_id: "bench".into(),
        root,
        view,
        width: 2560.0,
        height: 1440.0,
        dpr: 1.5,
        transform: TransformDto {
            scale: 1.0,
            tx: 0.0,
            ty: 0.0,
        },
        size_mode: SizeMode::Allocated,
        style: Style::Flat,
        filters: ViewFilters::default(),
        animate: true,
    }
}

/// Median of `runs` request → frame-sent times for `view` at `root`.
fn time_layout(data: &VolumeData, root: u32, view: View, runs: u32) -> (Duration, usize) {
    let bytes = Arc::new(Mutex::new(0usize));
    let b = bytes.clone();
    let stream = LayoutStream::new(Box::new(move |f| {
        *b.lock().unwrap() = f.len();
        Ok(())
    }));
    let mut times = Vec::new();
    for seq in 1..=runs {
        let t = Instant::now();
        stream.announce(seq);
        stream.serve(data, &request(root, view), seq).unwrap();
        times.push(t.elapsed());
    }
    times.sort();
    let size = *bytes.lock().unwrap();
    (times[times.len() / 2], size)
}

fn report_layouts(data: &VolumeData) {
    let ix = &data.index;
    println!("entries: {}", ix.len());
    let mem = ix.memory_report();
    println!(
        "index memory: {:.1} B/entry excl. names ({} MiB total incl. names); app side tables 8 B/entry",
        mem.bytes_per_entry(),
        (mem.total_excluding_names() + mem.name_bytes) >> 20
    );
    let root = data.root_wire();
    for view in [
        View::Treemap,
        View::Sunburst,
        View::Icicle,
        View::Bubbles,
        View::Mindmap,
    ] {
        let (t, size) = time_layout(data, root, view, 9);
        println!(
            "layout root {view:?}: {:.2} ms, frame {} KiB",
            t.as_secs_f64() * 1e3,
            size >> 10
        );
    }
    let target = (0..ix.slot_count() as u32)
        .map(EntryId)
        .filter(|&e| ix.is_live(e) && ix.is_dir(e) && e != ix.root())
        .min_by_key(|&e| data.items(e).abs_diff(100_000));
    if let Some(d) = target {
        let (t, size) = time_layout(data, data.wire(d), View::Treemap, 9);
        println!(
            "layout subtree of {} entries, treemap: {:.2} ms, frame {} KiB",
            data.items(d),
            t.as_secs_f64() * 1e3,
            size >> 10
        );
    }
}

fn real(folder: &str, engine: Arc<Engine>) {
    let slot: Arc<DataSlot> = Arc::new(RwLock::new(None));
    let started = Instant::now();
    let obs = FirstPreview {
        started,
        slot: &slot,
        at: Mutex::new(None),
    };
    let target = ScanTarget {
        root: folder.into(),
        root_display: folder.to_owned(),
        volume: None,
    };
    let mut ingest = Ingest::new(engine, target, slot.clone(), ScannerUsed::Walker, true);
    let stats = walk(
        &mut ingest,
        strata_walk::WalkOptions::default(),
        &strata_walk::CancelToken::new(),
        &obs,
    )
    .unwrap();
    let scanned = started.elapsed();
    if let Some((shown, framed, n)) = *obs.at.lock().unwrap() {
        println!(
            "scan start → first preview ({n} entries): {:.0} ms; → its first treemap frame: {:.0} ms",
            shown.as_secs_f64() * 1e3,
            framed.as_secs_f64() * 1e3
        );
    }
    let t = Instant::now();
    let f = ingest
        .finish(stats.partial, ShadowBytes::default())
        .unwrap();
    let finish = t.elapsed();
    println!(
        "walk: {} dirs, {} files in {:.2} s (partial: {}); finish: {:.0} ms \
         (preview stop {:.0}, build {:.0}, classify {:.0}, remap {:.0})",
        stats.totals.dirs,
        stats.totals.files,
        scanned.as_secs_f64(),
        stats.partial,
        finish.as_secs_f64() * 1e3,
        f.preview_drain.as_secs_f64() * 1e3,
        f.build.as_secs_f64() * 1e3,
        f.classify.as_secs_f64() * 1e3,
        f.remap.as_secs_f64() * 1e3,
    );
    let g = slot.read().unwrap();
    report_layouts(g.as_ref().unwrap());
}

fn synthetic(n: u64, engine: &Engine) {
    let t = Instant::now();
    let mut b = IndexBuilder::new(IndexOptions::default());
    let r = |i: u64| FileRef::from_parts(i, 1);
    let rec = |id: u64, parent: u64, name: String, size: u64, dir: bool| ScanRecord {
        id: r(id),
        links: vec![NameLink {
            parent: r(parent),
            name: WideName::from_str_lossless(&name),
        }],
        attributes: 0,
        flags: if dir {
            EntryFlags::DIR
        } else {
            EntryFlags::EMPTY
        },
        times: Times {
            modified: FileTime::from_unix_secs(1_700_000_000 + (id as i64 % 10_000_000)),
            ..Times::default()
        },
        fn_created: None,
        sizes: Sizes {
            logical: size,
            allocated: size.div_ceil(4096) * 4096,
            ..Sizes::default()
        },
        reparse: None,
        ads: vec![],
    };
    b.push(rec(5, 5, String::new(), 0, true)).unwrap();
    // Fan-out 16 directories; 1 in 8 entries is a directory.
    let mut dirs = vec![5u64];
    let mut next = 100u64;
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    while next < n + 100 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let parent = dirs[(seed % dirs.len() as u64) as usize];
        let id = next;
        next += 1;
        if seed.is_multiple_of(8) {
            b.push(rec(id, parent, format!("dir{id}"), 0, true))
                .unwrap();
            dirs.push(id);
        } else {
            let ext = ["dll", "js", "png", "mp4", "txt", "gguf"][(seed >> 20) as usize % 6];
            b.push(rec(
                id,
                parent,
                format!("file{id}.{ext}"),
                (seed >> 8) % 50_000_000,
                false,
            ))
            .unwrap();
        }
    }
    let mut index = b.finish().unwrap();
    let built = t.elapsed();
    let t = Instant::now();
    let slots = ext_slots(&index);
    let c = classify_index(engine, &mut index, r"X:\", &slots);
    println!(
        "synthetic {n}: build {:.0} ms, classify {:.0} ms",
        built.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3
    );
    let data = VolumeData::new(0, index, c, r"X:\".into(), ScannerUsed::Walker);
    report_layouts(&data);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kf = strata_win::known::known_folders().unwrap_or_default();
    let t = Instant::now();
    let engine = Arc::new(Engine::new(&kf, None).expect("rules compile"));
    println!(
        "rules compiled in {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    engine.load_catalog(&kf);
    println!(
        "app catalog read in {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    match args.as_slice() {
        [flag, n] if flag == "--synthetic" => synthetic(n.parse().expect("entry count"), &engine),
        [folder] => real(folder, engine),
        _ => eprintln!("usage: pipeline_bench <folder> | --synthetic <entries>"),
    }
}
