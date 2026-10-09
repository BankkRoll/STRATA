//! Totals, top-N largest files, reconciliation and their text rendering.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fmt::Write as _;

use strata_core::{EntryFlags, ScanRecord};
use strata_ntfs::ScanStats;

use crate::paths::PathIndex;

/// Sums over every emitted record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Totals {
    /// Non-directory records.
    pub files: u64,
    /// Directory records.
    pub dirs: u64,
    /// Σ unnamed-stream logical sizes.
    pub logical: u64,
    /// Σ unnamed-stream allocated sizes.
    pub allocated: u64,
    /// Σ ADS logical sizes.
    pub ads_logical: u64,
    /// Σ ADS allocated sizes.
    pub ads_allocated: u64,
    /// Σ directory index allocations.
    pub dir_overhead: u64,
    /// Σ other non-resident attribute allocations (attribute lists, bitmaps, EAs).
    pub attr_overhead: u64,
    /// Σ total allocated of NTFS metadata records (already included above).
    pub ntfs_metadata_allocated: u64,
    /// Files with more than one link.
    pub hardlinked_files: u64,
    /// Records with a reparse point.
    pub reparse_points: u64,
}

impl Totals {
    /// Σ allocated + ADS allocated + directory and attribute overhead.
    #[must_use]
    pub fn sum_allocated(&self) -> u64 {
        self.allocated
            .saturating_add(self.ads_allocated)
            .saturating_add(self.dir_overhead)
            .saturating_add(self.attr_overhead)
    }

    fn add(&mut self, r: &ScanRecord) {
        if r.is_dir() {
            self.dirs += 1;
        } else {
            self.files += 1;
        }
        let s = &r.sizes;
        self.logical = self.logical.saturating_add(s.logical);
        self.allocated = self.allocated.saturating_add(s.allocated);
        self.ads_logical = self.ads_logical.saturating_add(s.ads_logical);
        self.ads_allocated = self.ads_allocated.saturating_add(s.ads_allocated);
        self.dir_overhead = self.dir_overhead.saturating_add(s.dir_overhead);
        self.attr_overhead = self.attr_overhead.saturating_add(s.attr_overhead);
        if r.flags.contains(EntryFlags::NTFS_METADATA) {
            self.ntfs_metadata_allocated = self
                .ntfs_metadata_allocated
                .saturating_add(s.total_allocated());
        }
        if r.links.len() > 1 && !r.is_dir() {
            self.hardlinked_files += 1;
        }
        if r.reparse.is_some() {
            self.reparse_points += 1;
        }
    }
}

/// Consumes scan batches and keeps only what the report needs: directory
/// names for paths, totals, the N largest files, and (for JSON) every record.
#[derive(Debug)]
pub struct Collector {
    top_n: usize,
    heap: BinaryHeap<Reverse<(u64, Reverse<u64>)>>,
    top: HashMap<u64, ScanRecord>,
    /// Directory map for path building.
    pub paths: PathIndex,
    /// Running totals.
    pub totals: Totals,
    /// Every record, when `keep_all` was requested.
    pub all: Option<Vec<ScanRecord>>,
}

impl Collector {
    /// A collector keeping the `top_n` largest files, and every record if `keep_all`.
    #[must_use]
    pub fn new(top_n: usize, keep_all: bool) -> Self {
        Self {
            top_n,
            heap: BinaryHeap::new(),
            top: HashMap::new(),
            paths: PathIndex::default(),
            totals: Totals::default(),
            all: keep_all.then(Vec::new),
        }
    }

    /// Adds one record.
    pub fn add(&mut self, r: ScanRecord) {
        self.totals.add(&r);
        self.paths.add(&r);
        if !r.is_dir() && self.top_n > 0 {
            let key = (r.sizes.total_allocated(), Reverse(r.id.record()));
            if self.heap.len() < self.top_n {
                self.heap.push(Reverse(key));
                self.top.insert(r.id.record(), r.clone());
            } else if self.heap.peek().is_some_and(|Reverse(min)| key > *min) {
                if let Some(Reverse((_, Reverse(evicted)))) = self.heap.pop() {
                    self.top.remove(&evicted);
                }
                self.heap.push(Reverse(key));
                self.top.insert(r.id.record(), r.clone());
            }
        }
        if let Some(all) = &mut self.all {
            all.push(r);
        }
    }

    /// The largest files, largest first (ties by record number).
    #[must_use]
    pub fn largest(&self) -> Vec<&ScanRecord> {
        let mut v: Vec<&ScanRecord> = self.top.values().collect();
        v.sort_by_key(|r| (Reverse(r.sizes.total_allocated()), r.id.record()));
        v
    }
}

/// Used-space reconciliation (SPEC §5, §7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// Bytes the volume reports as used, if known.
    pub used: Option<u64>,
    /// Where `used` came from.
    pub used_source: String,
    /// Σ allocated over all records, including attribute overhead.
    pub files_allocated: u64,
    /// Of `files_allocated`, bytes in non-content attributes.
    pub attr_overhead: u64,
    /// Whether the target is a live volume (enables live-only explanations).
    pub live_volume: bool,
}

impl Reconciliation {
    /// Everything the scan accounted for.
    #[must_use]
    pub fn accounted(&self) -> u64 {
        self.files_allocated
    }

    /// `used - accounted`, signed.
    #[must_use]
    pub fn unaccounted(&self) -> Option<i128> {
        self.used
            .map(|u| i128::from(u) - i128::from(self.accounted()))
    }
}

/// Human-readable binary size (`1.50 GiB`).
#[must_use]
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

fn row(out: &mut String, label: &str, bytes: u64) {
    let _ = writeln!(out, "  {label:<28} {:>14}  ({bytes} bytes)", human(bytes));
}

/// Renders the full text report.
#[must_use]
pub fn render(
    stats: &ScanStats,
    collector: &mut Collector,
    recon: &Reconciliation,
    path_prefix: &str,
) -> String {
    let mut o = String::new();
    let secs = stats.elapsed.as_secs_f64();
    let _ = writeln!(o, "Scan");
    let _ = writeln!(o, "  elapsed                      {secs:.3} s");
    let _ = writeln!(
        o,
        "  records                      {} total, {} in use, {} free ({} skipped by bitmap)",
        stats.records_total, stats.in_use, stats.free, stats.skipped_by_bitmap
    );
    let _ = writeln!(
        o,
        "  problems                     {} BAAD, {} torn, {} bad signature, {} malformed, {} unreadable",
        stats.corrupt, stats.torn, stats.bad_signature, stats.malformed, stats.unreadable
    );
    let _ = writeln!(
        o,
        "  extension records            {} seen, {} merged, {} orphaned",
        stats.extension_records, stats.extensions_merged, stats.extensions_orphaned
    );
    let _ = writeln!(
        o,
        "  emitted                      {} records, {} read",
        stats.records_emitted,
        human(stats.bytes_read)
    );
    if secs > 0.0 {
        let _ = writeln!(
            o,
            "  throughput                   {:.0} records/s",
            stats.records_emitted as f64 / secs
        );
    }
    if stats.cancelled {
        let _ = writeln!(o, "  CANCELLED: totals are partial");
    }

    let t = collector.totals;
    let _ = writeln!(o, "\nTotals");
    let _ = writeln!(o, "  files                        {}", t.files);
    let _ = writeln!(o, "  directories                  {}", t.dirs);
    let _ = writeln!(o, "  hardlinked files             {}", t.hardlinked_files);
    let _ = writeln!(o, "  reparse points               {}", t.reparse_points);
    row(&mut o, "logical", t.logical);
    row(&mut o, "allocated", t.allocated);
    row(&mut o, "ADS logical", t.ads_logical);
    row(&mut o, "ADS allocated", t.ads_allocated);
    row(&mut o, "directory index overhead", t.dir_overhead);
    row(
        &mut o,
        "NTFS metadata (included)",
        t.ntfs_metadata_allocated,
    );

    let largest: Vec<ScanRecord> = collector.largest().into_iter().cloned().collect();
    let _ = writeln!(o, "\nLargest {} files by allocated size", largest.len());
    for r in &largest {
        let path = r.links.first().map_or_else(
            || format!("{}<orphan>\\#{}", path_prefix, r.id.record()),
            |l| format!("{path_prefix}{}", collector.paths.path(l.parent, &l.name)),
        );
        let extra = if r.links.len() > 1 {
            format!("  (+{} more links)", r.links.len() - 1)
        } else {
            String::new()
        };
        let _ = writeln!(
            o,
            "  {:>12}  {:>12}  {path}{extra}",
            human(r.sizes.total_allocated()),
            human(r.sizes.total_logical())
        );
    }

    let _ = writeln!(o, "\nReconciliation");
    match recon.used {
        Some(used) => row(&mut o, &format!("used ({})", recon.used_source), used),
        None => {
            let _ = writeln!(
                o,
                "  used                         unavailable ({})",
                recon.used_source
            );
        }
    }
    row(&mut o, "sum allocated (records)", recon.files_allocated);
    row(&mut o, "  of which attribute overhead", recon.attr_overhead);
    if let Some(gap) = recon.unaccounted() {
        let sign = if gap < 0 { "-" } else { "" };
        let mag = u64::try_from(gap.unsigned_abs()).unwrap_or(u64::MAX);
        let _ = writeln!(
            o,
            "  {:<28} {sign}{:>13}  ({gap} bytes)",
            "Unaccounted",
            human(mag)
        );
        let _ = writeln!(o, "  Known likely causes:");
        for c in causes(stats, recon, gap) {
            let _ = writeln!(o, "    - {c}");
        }
    }
    o
}

/// Explanations for a reconciliation gap, most likely first.
fn causes(stats: &ScanStats, recon: &Reconciliation, gap: i128) -> Vec<String> {
    let mut c = Vec::new();
    if gap == 0 {
        c.push("none: every used cluster is attributed to a record".to_owned());
        return c;
    }
    if recon.live_volume {
        c.push(
            "Volume Shadow Copy storage (System Restore): lives outside the MFT; see \
             `vssadmin list shadowstorage`"
                .to_owned(),
        );
        c.push(
            "files created, grown or deleted while the scan ran ($LogFile, $UsnJrnl, pagefile)"
                .to_owned(),
        );
    }
    let skipped =
        stats.corrupt + stats.torn + stats.bad_signature + stats.malformed + stats.unreadable;
    if skipped > 0 {
        c.push(format!(
            "{skipped} MFT records skipped as corrupt or unreadable"
        ));
    }
    if stats.extensions_orphaned > 0 {
        c.push(format!(
            "{} extension records with no valid base (their clusters are not attributed)",
            stats.extensions_orphaned
        ));
    }
    if stats.cancelled {
        c.push("scan was cancelled: totals are partial".to_owned());
    }
    if gap < 0 {
        c.push(
            "free-space accounting lags (clusters freed but not yet released, or quota-limited \
             free-space reporting)"
                .to_owned(),
        );
    } else {
        c.push(
            "free-space bitmap rounding and clusters reserved by NTFS outside any file".to_owned(),
        );
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::{FileRef, Sizes, Times};

    fn file(n: u64, alloc: u64) -> ScanRecord {
        ScanRecord {
            id: FileRef::from_parts(n, 1),
            links: vec![],
            attributes: 0,
            flags: EntryFlags::EMPTY,
            times: Times::default(),
            fn_created: None,
            sizes: Sizes {
                allocated: alloc,
                logical: alloc,
                ..Sizes::default()
            },
            reparse: None,
            ads: vec![],
        }
    }

    #[test]
    fn keeps_the_n_largest_with_stable_ties() {
        let mut c = Collector::new(3, false);
        for (n, a) in [(1, 10), (2, 50), (3, 50), (4, 5), (5, 70), (6, 50)] {
            c.add(file(n, a));
        }
        let got: Vec<u64> = c.largest().iter().map(|r| r.id.record()).collect();
        assert_eq!(got, vec![5, 2, 3]);
        assert_eq!(c.totals.files, 6);
        assert_eq!(c.totals.allocated, 235);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1536), "1.50 KiB");
        assert_eq!(human(u64::MAX), "16384.00 PiB");
    }
}
