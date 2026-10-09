//! One indexed volume as the commands see it.
//!
//! [`VolumeData`] bundles the [`Index`] with the app-side side tables the
//! index does not hold: per-entry [`PackedClass`] bits, the static part of
//! each entry's packed color key, the file-type slot table and the remap of
//! the previous generation's ids (see [`crate::ids`]). It also implements
//! the per-entry queries shared by layout, rows, tooltips and search.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use strata_classify::{Classification, PackedClass};
use strata_core::{EntryFlags, EpochSecs, FileTime, SizeMode};
use strata_index::{EntryId, Index};

use crate::classify::{Classified, UNCLASSIFIED};
use crate::ids::{self, Remap};

/// Which scanner produced an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScannerUsed {
    /// Raw MFT through the elevated helper.
    Mft,
    /// The unelevated directory walker.
    Walker,
}

/// Seconds since 2000-01-01 UTC for "now".
#[must_use]
pub fn now_epoch2000() -> u32 {
    EpochSecs::from_filetime(now_filetime()).0
}

/// The current time as a FILETIME.
#[must_use]
pub fn now_filetime() -> FileTime {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    FileTime::from_unix_secs(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Unix milliseconds of an index timestamp (`None` for 0 = unknown).
#[must_use]
pub fn epoch2000_to_ms(secs: u32) -> Option<i64> {
    (secs != 0).then(|| EpochSecs(secs).to_filetime().to_unix_secs() * 1000)
}

/// Upper bounds (seconds of age) of the age buckets 1–19; older is 20.
/// Mirrors `AGE_BUCKETS` in `ui/src/lib/palette.ts`.
const AGE_LIMITS: [u32; 19] = [
    3600,
    6 * 3600,
    86_400,
    3 * 86_400,
    7 * 86_400,
    14 * 86_400,
    30 * 86_400,
    60 * 86_400,
    91 * 86_400,
    182 * 86_400,
    274 * 86_400,
    365 * 86_400,
    548 * 86_400,
    730 * 86_400,
    1095 * 86_400,
    1461 * 86_400,
    1826 * 86_400,
    2557 * 86_400,
    3652 * 86_400,
];

/// Age bucket code of the color key: 0 unknown, 1–20 by age, 31 suspicious.
#[must_use]
pub fn age_bucket(modified: u32, now: u32, suspicious: bool) -> u32 {
    if suspicious {
        return 31;
    }
    if modified == 0 {
        return 0;
    }
    let age = now.saturating_sub(modified);
    AGE_LIMITS
        .iter()
        .position(|&limit| age < limit)
        .map_or(20, |i| i as u32 + 1)
}

/// The filters every view applies (`ViewFilters` in `ui/src/lib/types.ts`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ViewFilters {
    /// Wire ids hidden with "Exclude from view".
    pub excluded: Vec<u32>,
    /// Category ids to show; empty = all.
    pub categories: Vec<u16>,
    /// Hide entries smaller than this (0 = off).
    pub min_bytes: u64,
    /// Only entries modified within this many days.
    pub modified_within_days: Option<u32>,
}

/// [`ViewFilters`] resolved against one index.
///
/// Filters hide entries; directory sizes keep their full subtree totals, so
/// a folder whose children are all hidden keeps its area (layout sees it as
/// a leaf-like block).
#[derive(Debug, Clone, Default)]
pub struct ResolvedFilters {
    excluded: HashSet<u32>,
    categories: Vec<u16>,
    min_bytes: u64,
    modified_since: Option<u32>,
    mode: SizeMode,
}

impl ResolvedFilters {
    /// Whether nothing is filtered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.excluded.is_empty()
            && self.categories.is_empty()
            && self.min_bytes == 0
            && self.modified_since.is_none()
    }

    /// Whether `id` passes.
    #[must_use]
    pub fn keep(&self, data: &VolumeData, id: EntryId) -> bool {
        // NOTE: the index always has its "Orphaned entries" and "NTFS
        // metadata" group nodes, so live and fresh indexes agree; empty
        // ones are not shown.
        let f = data.index.flags(id);
        if f.contains(EntryFlags::VIRTUAL)
            && f.contains(EntryFlags::DIR)
            && data.index.child_count(id) == 0
        {
            return false;
        }
        if self.is_empty() {
            return true;
        }
        if self.excluded.contains(&id.0) {
            return false;
        }
        let ix = &data.index;
        if self.min_bytes > 0 && ix.size(id, self.mode) < self.min_bytes {
            return false;
        }
        if let Some(since) = self.modified_since
            && data.modified(id) < since
        {
            return false;
        }
        if !self.categories.is_empty() {
            let cat = (data.key_static(id) & 0xF) as u16;
            if !self.categories.contains(&cat) {
                return false;
            }
        }
        true
    }
}

/// An indexed volume.
#[derive(Debug)]
pub struct VolumeData {
    /// Id generation (see [`crate::ids`]).
    pub generation: u8,
    /// The index.
    pub index: Index,
    /// [`PackedClass`] bits per entry, [`UNCLASSIFIED`] when unknown.
    pub classes: Vec<u32>,
    /// Static color key bits per entry.
    pub keys: Vec<u32>,
    /// Translation of the previous generation's ids.
    pub remap: Option<Remap>,
    /// Display path of the root (`C:\`, or a folder).
    pub root_path: String,
    /// Scanner that produced the index.
    pub scanner: ScannerUsed,
    /// The scan was cancelled or incomplete.
    pub partial: bool,
    /// Still streaming (a scan preview).
    pub preview: bool,
    /// When the scan behind it finished (Unix ms), or started for a preview.
    pub scanned_at_ms: i64,
    /// Allocated bytes per category id, computed once per index (the volume
    /// list is re-sent on every progress tick).
    pub category_bytes: std::collections::BTreeMap<String, u64>,
    /// Entries changed by live updates or cleanup, with when (seconds since
    /// 2000), for the "changed recently" bit of the color key.
    pub recent: std::collections::HashMap<u32, u32>,
    /// Unix ms of the last change applied after the scan (0 = none).
    pub changed_ms: i64,
}

/// How long an entry keeps the "changed recently" color-key bit.
pub const RECENT_SECS: u32 = 10 * 60;

/// The "changed recently" bit of the packed color key.
pub const RECENT_BIT: u32 = 1 << 30;

impl VolumeData {
    /// Wraps a classified index.
    #[must_use]
    pub fn new(
        generation: u8,
        index: Index,
        classified: Classified,
        root_path: String,
        scanner: ScannerUsed,
    ) -> Self {
        let category_bytes = index
            .category_breakdown(None, SizeMode::Allocated)
            .into_iter()
            .map(|b| (b.id.to_string(), b.bytes))
            .collect();
        Self {
            category_bytes,
            generation: generation & 3,
            index,
            classes: classified.classes,
            keys: classified.keys,
            remap: None,
            root_path,
            scanner,
            partial: false,
            preview: false,
            scanned_at_ms: unix_ms(),
            recent: std::collections::HashMap::new(),
            changed_ms: 0,
        }
    }

    /// Applies live updates (journal, folder watch, cleanup) to the index,
    /// keeps the side tables in step, and marks what changed as recent.
    /// New entries stay unclassified until the next classification pass.
    ///
    /// # Errors
    ///
    /// The index's error; updates before the failing one stay applied.
    pub fn apply(
        &mut self,
        updates: impl IntoIterator<Item = strata_index::Update>,
    ) -> Result<strata_index::ChangeSet, strata_index::IndexError> {
        let r = self.index.apply(updates);
        self.after_change(r.as_ref().ok());
        r
    }

    /// Records a change set produced directly on the index.
    pub fn after_change(&mut self, changes: Option<&strata_index::ChangeSet>) {
        let slots = self.index.slot_count();
        if self.classes.len() < slots {
            self.classes.resize(slots, UNCLASSIFIED);
            self.keys.resize(slots, 0);
        }
        let now = now_epoch2000();
        self.recent
            .retain(|_, at| now.saturating_sub(*at) < RECENT_SECS);
        if let Some(c) = changes {
            for id in c.created.iter().chain(&c.updated) {
                self.recent.insert(id.0, now);
            }
            for id in &c.removed {
                self.recent.remove(&id.0);
            }
            for (id, _) in &c.aggregates {
                self.recent.insert(id.0, now);
            }
        }
        self.changed_ms = unix_ms();
        self.category_bytes = self
            .index
            .category_breakdown(None, SizeMode::Allocated)
            .into_iter()
            .map(|b| (b.id.to_string(), b.bytes))
            .collect();
    }

    /// Wire id of an index entry.
    #[must_use]
    pub const fn wire(&self, id: EntryId) -> u32 {
        ids::encode(self.generation, id)
    }

    /// Resolves a wire id (current or previous generation) to a live entry.
    #[must_use]
    pub fn resolve(&self, wire: u32) -> Option<EntryId> {
        let (generation, local) = ids::decode(wire);
        let local = if generation == self.generation {
            local
        } else {
            let r = self.remap.as_ref()?;
            if r.generation != generation {
                return None;
            }
            r.get(local)?
        };
        let id = EntryId(local);
        self.index.is_live(id).then_some(id)
    }

    /// The root's wire id.
    #[must_use]
    pub fn root_wire(&self) -> u32 {
        self.wire(self.index.root())
    }

    /// Classification of an entry, if classified.
    #[must_use]
    pub fn class(&self, id: EntryId) -> Option<Classification> {
        let c = *self.classes.get(id.index())?;
        (c != UNCLASSIFIED).then(|| PackedClass(c).unpack())
    }

    /// Raw class bits ([`UNCLASSIFIED`] when unknown).
    #[must_use]
    pub fn class_bits(&self, id: EntryId) -> u32 {
        self.classes
            .get(id.index())
            .copied()
            .unwrap_or(UNCLASSIFIED)
    }

    /// Static color key bits (0 when unknown).
    #[must_use]
    pub fn key_static(&self, id: EntryId) -> u32 {
        self.keys.get(id.index()).copied().unwrap_or(0)
    }

    /// Modification time used for display, sort and age: the subtree's
    /// newest for directories, the entry's own otherwise (0 = unknown).
    #[must_use]
    pub fn modified(&self, id: EntryId) -> u32 {
        if self.index.is_dir(id) {
            return self
                .index
                .aggregate(id)
                .and_then(|a| a.newest)
                .map_or(0, |t| t.0);
        }
        self.index.times(id).map_or(0, |t| t.modified.0)
    }

    /// Full packed color key at time `now` (seconds since 2000).
    #[must_use]
    pub fn color_key(&self, id: EntryId, now: u32) -> u32 {
        let suspicious = self.index.flags(id).contains(EntryFlags::SUSPICIOUS_TIME);
        let recent = self
            .recent
            .get(&id.0)
            .is_some_and(|&at| now.saturating_sub(at) < RECENT_SECS);
        self.key_static(id)
            | age_bucket(self.modified(id), now, suspicious) << 7
            | if recent { RECENT_BIT } else { 0 }
    }

    /// Items below a directory (files + directories), 0 for files.
    #[must_use]
    pub fn items(&self, id: EntryId) -> u64 {
        self.index
            .aggregate(id)
            .map_or(0, |a| u64::from(a.files) + u64::from(a.dirs))
    }

    /// Display path (Win32, no `\\?\`).
    #[must_use]
    pub fn path(&self, id: EntryId) -> String {
        self.index.path_string(id)
    }

    /// Resolves the UI's filters for this index.
    #[must_use]
    pub fn resolve_filters(&self, f: &ViewFilters, mode: SizeMode) -> ResolvedFilters {
        let now = now_epoch2000();
        ResolvedFilters {
            excluded: f
                .excluded
                .iter()
                .filter_map(|&w| self.resolve(w))
                .map(|e| e.0)
                .collect(),
            categories: f.categories.clone(),
            min_bytes: f.min_bytes,
            modified_since: f
                .modified_within_days
                .map(|d| now.saturating_sub(d.saturating_mul(86_400))),
            mode,
        }
    }
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_buckets_match_ui_thresholds() {
        let now = 1_000_000_000;
        assert_eq!(age_bucket(0, now, false), 0);
        assert_eq!(age_bucket(now, now, false), 1);
        assert_eq!(age_bucket(now - 3599, now, false), 1);
        assert_eq!(age_bucket(now - 3600, now, false), 2);
        assert_eq!(age_bucket(now - 86_400, now, false), 4);
        assert_eq!(age_bucket(now - 3651 * 86_400, now, false), 19);
        assert_eq!(age_bucket(now - 3652 * 86_400, now, false), 20);
        assert_eq!(
            age_bucket(now + 50, now, false),
            1,
            "future clamps to newest"
        );
        assert_eq!(age_bucket(now, now, true), 31);
    }

    #[test]
    fn epoch_conversion() {
        assert_eq!(epoch2000_to_ms(0), None);
        assert_eq!(epoch2000_to_ms(1), Some(946_684_801_000));
    }
}
