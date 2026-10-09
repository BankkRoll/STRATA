//! Real-volume benchmark harness for the walker.
//!
//! ```text
//! cargo run --release -p strata-walk --example walkbench -- <root> [dirinfo|find] [alloc 0|1] [threads]
//! ```
//!
//! Set `STRATA_CANCEL_AFTER_MS` to cancel mid-walk and report how long `run`
//! took to return after the cancel.
//!
//! Prints one line of `key=value` pairs: elapsed time, records, throughput,
//! extrapolated time per million files, errors, totals and the volume's used
//! space for reconciliation.

use std::time::Instant;

use strata_walk::{CancelToken, FnSink, ListingMethod, WalkOptions, Walker};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| r"C:\".to_owned());
    let listing = match args.next().as_deref() {
        Some("find") => ListingMethod::FindFirstFile,
        _ => ListingMethod::DirectoryInfo,
    };
    let allocation_pass = args.next().is_none_or(|a| a != "0");
    let mut opts = WalkOptions {
        listing,
        allocation_pass,
        ..WalkOptions::default()
    };
    if let Some(t) = args.next().and_then(|t| t.parse().ok()) {
        opts.threads = t;
    }
    let threads = opts.threads;
    let walker = Walker::new(&root, opts).expect("walker");

    let mut records = 0u64;
    let mut sink = FnSink::new(|b: Vec<strata_core::ScanRecord>| records += b.len() as u64);
    let cancel = CancelToken::new();
    let cancelled_at = std::sync::Arc::new(std::sync::Mutex::new(None));
    if let Some(ms) = std::env::var("STRATA_CANCEL_AFTER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        let (c, at) = (cancel.clone(), std::sync::Arc::clone(&cancelled_at));
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            *at.lock().expect("lock") = Some(Instant::now());
            c.cancel();
        });
    }
    let started = Instant::now();
    let stats = walker.run(&mut sink, &cancel).expect("walk");
    let secs = started.elapsed().as_secs_f64();
    if let Some(at) = *cancelled_at.lock().expect("lock") {
        println!(
            "cancel_to_return_ms={:.1}",
            at.elapsed().as_secs_f64() * 1e3
        );
    }

    let t = stats.totals;
    let per_sec = (t.files + t.dirs) as f64 / secs;
    let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
    let used = stats.volume.as_ref().map_or(0, |v| v.used_bytes());
    println!(
        "root={root} listing={listing:?} alloc={allocation_pass} threads={threads} \
         secs={secs:.2} files={} dirs={} records={records} entries_per_sec={per_sec:.0} \
         secs_per_1M_entries={:.2} access_denied_dirs={} partial_dirs={} estimated={} \
         hardlinks_merged={} errors={:?} logical_gib={:.2} allocated_gib={:.2} volume_used_gib={:.2} \
         fallback={}",
        t.files,
        t.dirs,
        1e6 / per_sec,
        stats.access_denied_dirs,
        stats.partial_dirs,
        stats.estimated_allocations,
        stats.hardlinks_merged,
        stats.errors.iter().collect::<Vec<_>>(),
        gib(t.logical_bytes),
        gib(t.allocated_bytes),
        gib(used),
        stats.dir_info_fallback,
    );
}
