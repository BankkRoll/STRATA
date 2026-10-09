//! Scan results: duplicate groups, statistics and progress events.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use strata_clean::Expected;
use strata_core::{FileRef, FileTime};

use crate::gate::{Exclusion, SkipReason};
use crate::keep::{KeepContext, KeepInput, KeepSuggestion, suggest_keep};

/// One copy in a duplicate group, as a handle showed it while hashing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DupFile {
    /// Volume serial (64-bit key from the candidate).
    pub volume_serial: u64,
    /// File reference, verified against the handle.
    pub file_ref: FileRef,
    /// Full path.
    pub path: PathBuf,
    /// Logical size of the unnamed stream.
    pub size: u64,
    /// Last-write time (full precision, from the handle).
    pub mtime: FileTime,
    /// Hardlink count seen on the handle.
    pub links: u32,
}

impl DupFile {
    /// What `strata-clean` must find on disk before deleting this copy.
    #[must_use]
    pub fn expected(&self) -> Expected {
        Expected {
            file_ref: self.file_ref,
            is_dir: false,
            size: self.size,
            modified: self.mtime,
        }
    }
}

/// Files with identical content (same size and BLAKE3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    /// Position in [`DuplicateReport::groups`]; stable for this report.
    pub id: u64,
    /// Size of each copy.
    pub size: u64,
    /// BLAKE3 of the content.
    pub hash: [u8; 32],
    /// The copies, sorted by path. Always at least two.
    pub files: Vec<DupFile>,
    /// Suggested copy to keep.
    pub keep: KeepSuggestion,
}

impl DuplicateGroup {
    /// Bytes freed by keeping one copy: `size × (copies − 1)`.
    #[must_use]
    pub fn wasted_bytes(&self) -> u64 {
        self.size
            .saturating_mul(self.files.len().saturating_sub(1) as u64)
    }

    /// Hex form of [`DuplicateGroup::hash`].
    #[must_use]
    pub fn hash_hex(&self) -> String {
        self.hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub(crate) fn suggest(&mut self, ctx: &KeepContext) {
        let inputs: Vec<KeepInput<'_>> = self
            .files
            .iter()
            .map(|f| KeepInput {
                path: &f.path,
                mtime: f.mtime,
            })
            .collect();
        self.keep = suggest_keep(&inputs, ctx);
    }
}

/// A file dropped after it was opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    /// Path.
    pub path: PathBuf,
    /// File reference from the candidate.
    pub file_ref: FileRef,
    /// Why.
    pub reason: SkipReason,
}

/// Counters for one scan.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScanStats {
    /// Candidates offered.
    pub candidates: u64,
    /// Candidates left out before opening, by reason.
    pub excluded: BTreeMap<Exclusion, u64>,
    /// Files whose metadata was read from a handle.
    pub measured: u64,
    /// Partial hashes computed.
    pub partial_hashed: u64,
    /// Partial hashes taken from the cache.
    pub partial_cached: u64,
    /// Full hashes computed.
    pub full_hashed: u64,
    /// Full hashes taken from the cache.
    pub full_cached: u64,
    /// Bytes read for hashing.
    pub bytes_read: u64,
    /// Files dropped after opening (see [`DuplicateReport::skipped`]).
    pub skipped: u64,
    /// Cache calls that failed (treated as misses).
    pub cache_errors: u64,
    /// Duplicate groups found.
    pub groups: u64,
    /// Files in duplicate groups.
    pub duplicate_files: u64,
    /// Sum of [`DuplicateGroup::wasted_bytes`].
    pub wasted_bytes: u64,
    /// Wall time in milliseconds.
    pub elapsed_ms: u64,
}

/// A completed scan.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DuplicateReport {
    /// Groups, largest [`DuplicateGroup::wasted_bytes`] first.
    pub groups: Vec<DuplicateGroup>,
    /// Counters.
    pub stats: ScanStats,
    /// Files dropped after opening, with reasons.
    pub skipped: Vec<SkippedFile>,
}

impl DuplicateReport {
    /// Recomputes every keep suggestion (after the user edits rules).
    pub fn apply_keep_context(&mut self, ctx: &KeepContext) {
        for g in &mut self.groups {
            g.suggest(ctx);
        }
    }

    /// The group with `id`.
    #[must_use]
    pub fn group(&self, id: u64) -> Option<&DuplicateGroup> {
        usize::try_from(id)
            .ok()
            .and_then(|i| self.groups.get(i))
            .filter(|g| g.id == id)
    }
}

/// How a scan ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanOutcome {
    /// Every phase finished.
    Completed(DuplicateReport),
    /// Cancelled. Hashes computed so far are in the cache, so the next scan
    /// resumes from them.
    Cancelled(ScanStats),
}

/// Pipeline phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Filtering candidates and grouping by size.
    Grouping,
    /// Reading size, time, identity and attributes from handles.
    Measuring,
    /// First/middle/last 64 KiB hashes.
    PartialHash,
    /// Full BLAKE3 hashes.
    FullHash,
    /// Done (or cancelled).
    Finished,
}

/// A progress event. Emitted at each phase start, at most every
/// [`crate::ScanConfig::progress_interval`] while working, and at the end.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Current phase.
    pub phase: Phase,
    /// Files finished in this phase.
    pub files_done: u64,
    /// Files this phase will process.
    pub files_total: u64,
    /// Bytes read in this phase.
    pub bytes_done: u64,
    /// Bytes this phase will read.
    pub bytes_total: u64,
    /// Read rate in this phase so far.
    pub bytes_per_sec: f64,
    /// Estimated seconds left in this phase, once a rate is known. The full
    /// hash phase dominates a scan, so its estimate is the useful one.
    pub eta_secs: Option<f64>,
    /// Hashes served by the cache so far (both phases).
    pub cache_hits: u64,
}
