//! Golden JSON format (`strata-golden/1`) for fixture comparisons.
//!
//! One entry per *path*: a file with N hardlinks yields N entries sharing a
//! `record`, distinguished by `link_index` (0 = the link that carries the
//! bytes: hardlinked data is counted once, at the first link discovered). Entries are sorted
//! by path, then link index, so files diff cleanly. The format is documented
//! in `tests/fixtures/README.md`.

use serde::{Deserialize, Serialize};
use strata_core::{CloudState, EntryFlags, ReparseKind, ScanRecord};

use crate::paths::PathIndex;
use crate::report::Totals;

/// Format identifier written into every file.
pub const FORMAT: &str = "strata-golden/1";

/// Top-level document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Golden {
    /// Always [`FORMAT`].
    pub format: String,
    /// Producer: `"mft"` for this scanner (the walker writes `"walk"`).
    pub source: String,
    /// Volume facts.
    pub volume: VolumeInfo,
    /// Totals over all records.
    pub totals: Totals,
    /// Per-path entries.
    pub entries: Vec<Entry>,
}

/// Volume facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// Bytes per cluster.
    pub cluster_size: u64,
    /// Bytes per MFT record.
    pub record_size: u32,
    /// Volume size from the boot sector.
    pub total_bytes: u64,
    /// Used bytes (volume API or `$Bitmap`), if known.
    pub used_bytes: Option<u64>,
    /// Non-resident attribute bytes outside file sizes.
    pub other_attr_allocated: u64,
    /// Volume serial number, hex.
    pub serial: String,
}

/// One alternate data stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// Stream name.
    pub name: String,
    /// Logical size.
    pub logical: u64,
    /// Allocated size.
    pub allocated: u64,
}

/// Reparse details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReparseInfo {
    /// Kind name (`symlink`, `mount-point`, `wof`, `cloud`, ...).
    pub kind: String,
    /// Raw tag, hex.
    pub tag: String,
    /// Display target, if any.
    pub target: Option<String>,
}

/// One path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Volume-relative path with `\` separators; `\` is the root.
    pub path: String,
    /// MFT record number.
    pub record: u64,
    /// `file` or `dir`.
    pub kind: String,
    /// Unnamed stream logical size.
    pub logical: u64,
    /// Unnamed stream allocated size.
    pub allocated: u64,
    /// Σ ADS logical.
    pub ads_logical: u64,
    /// Σ ADS allocated.
    pub ads_allocated: u64,
    /// Directory index allocation.
    pub dir_overhead: u64,
    /// Which of the record's links this path is (0-based).
    pub link_index: usize,
    /// Number of links of the record.
    pub link_count: usize,
    /// Flag names (see [`flag_names`]).
    pub flags: Vec<String>,
    /// Reparse point, if any.
    pub reparse: Option<ReparseInfo>,
    /// Cloud state (`online-only`, `locally-available`, `always-keep`).
    pub cloud: Option<String>,
    /// Alternate data streams.
    pub ads: Vec<Stream>,
}

/// Names of the boolean flags set in `f`, in a fixed order.
#[must_use]
pub fn flag_names(f: EntryFlags) -> Vec<String> {
    const NAMES: [(EntryFlags, &str); 11] = [
        (EntryFlags::DIR, "dir"),
        (EntryFlags::HIDDEN, "hidden"),
        (EntryFlags::SYSTEM, "system"),
        (EntryFlags::READONLY, "readonly"),
        (EntryFlags::COMPRESSED, "compressed"),
        (EntryFlags::SPARSE, "sparse"),
        (EntryFlags::ENCRYPTED, "encrypted"),
        (EntryFlags::HAS_ADS, "has-ads"),
        (EntryFlags::NTFS_METADATA, "ntfs-metadata"),
        (EntryFlags::TEMPORARY, "temporary"),
        (EntryFlags::OFFLINE, "offline"),
    ];
    NAMES
        .iter()
        .filter(|(bit, _)| f.contains(*bit))
        .map(|(_, n)| (*n).to_owned())
        .collect()
}

/// Kind name of a reparse kind.
#[must_use]
pub fn reparse_kind_name(k: ReparseKind) -> &'static str {
    match k {
        ReparseKind::None => "none",
        ReparseKind::Symlink => "symlink",
        ReparseKind::MountPoint => "mount-point",
        ReparseKind::Wof => "wof",
        ReparseKind::Cloud => "cloud",
        ReparseKind::Dedup => "dedup",
        ReparseKind::AppExecLink => "app-exec-link",
        ReparseKind::Wsl => "wsl",
        ReparseKind::Unknown => "unknown",
    }
}

fn cloud_name(c: CloudState) -> Option<String> {
    match c {
        CloudState::None => None,
        CloudState::OnlineOnly => Some("online-only".into()),
        CloudState::LocallyAvailable => Some("locally-available".into()),
        CloudState::AlwaysKeep => Some("always-keep".into()),
    }
}

/// Builds the per-path entries for `records`, sorted by path.
#[must_use]
pub fn entries(records: &[ScanRecord], paths: &mut PathIndex) -> Vec<Entry> {
    let mut out = Vec::with_capacity(records.len());
    for r in records {
        let base = |path: String, link_index: usize| Entry {
            path,
            record: r.id.record(),
            kind: if r.is_dir() { "dir" } else { "file" }.to_owned(),
            logical: r.sizes.logical,
            allocated: r.sizes.allocated,
            ads_logical: r.sizes.ads_logical,
            ads_allocated: r.sizes.ads_allocated,
            dir_overhead: r.sizes.dir_overhead,
            link_index,
            link_count: r.links.len(),
            flags: flag_names(r.flags),
            reparse: r.reparse.as_ref().map(|rp| ReparseInfo {
                kind: reparse_kind_name(r.flags.reparse()).to_owned(),
                tag: format!("0x{:08X}", rp.tag),
                target: rp.target.as_ref().map(|t| t.to_string_lossy()),
            }),
            cloud: cloud_name(r.flags.cloud()),
            ads: r
                .ads
                .iter()
                .map(|a| Stream {
                    name: a.name.to_string_lossy(),
                    logical: a.logical,
                    allocated: a.allocated,
                })
                .collect(),
        };
        if r.links.is_empty() {
            out.push(base(format!("<orphan>\\#{}", r.id.record()), 0));
        }
        for (i, l) in r.links.iter().enumerate() {
            out.push(base(paths.path(l.parent, &l.name), i));
        }
    }
    out.sort_by(|a, b| (&a.path, a.link_index, a.record).cmp(&(&b.path, b.link_index, b.record)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_and_kind_names() {
        let f = EntryFlags::DIR | EntryFlags::HAS_ADS;
        assert_eq!(flag_names(f), vec!["dir", "has-ads"]);
        assert_eq!(reparse_kind_name(ReparseKind::MountPoint), "mount-point");
        assert_eq!(
            cloud_name(CloudState::AlwaysKeep).as_deref(),
            Some("always-keep")
        );
    }
}
