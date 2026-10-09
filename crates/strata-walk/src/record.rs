//! Turning listing entries and per-file facts into [`ScanRecord`]s.

use strata_core::{
    AdsInfo, CloudState, EntryFlags, FileRef, FileTime, NameLink, Reparse, ReparseKind, ScanRecord,
    Sizes, Times, WideName, win32,
};

use crate::parse::{RawEntry, StreamEntry, is_wof_stream};

/// Attribute bits meaning "content is not (fully) local; touching data would
/// recall it from the provider".
pub(crate) const RECALL_BITS: u32 = win32::FILE_ATTRIBUTE_RECALL_ON_OPEN
    | win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    | win32::FILE_ATTRIBUTE_OFFLINE;

/// Reparse kind implied by attributes and tag.
pub(crate) fn reparse_kind(attributes: u32, tag: u32) -> ReparseKind {
    if attributes & win32::FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        ReparseKind::None
    } else {
        ReparseKind::from_tag(tag)
    }
}

/// Allocation estimate used until the allocation pass reports the real
/// value: logical size rounded up to whole clusters.
///
/// Content that is not local (cloud online-only, offline HSM) is estimated
/// at 0 instead, because rounding its logical size up would attribute
/// gigabytes of remote data to the local disk.
pub(crate) fn estimate_allocation(logical: u64, cluster: u64, attributes: u32) -> u64 {
    if attributes & RECALL_BITS != 0 || cluster == 0 {
        return 0;
    }
    logical.div_ceil(cluster).saturating_mul(cluster)
}

/// Flags derived from attributes, reparse tag and timestamps.
pub(crate) fn flags_for(attributes: u32, tag: u32, times: &Times, now: FileTime) -> EntryFlags {
    let kind = reparse_kind(attributes, tag);
    let mut flags = EntryFlags::from_win32_attributes(attributes).with_reparse(kind);
    if kind == ReparseKind::Cloud {
        flags = flags.with_cloud(CloudState::from_attributes(attributes));
    }
    if times.created.is_suspicious(now) || times.modified.is_suspicious(now) {
        flags |= EntryFlags::SUSPICIOUS_TIME;
    }
    flags
}

/// Volume facts and the clock used while building records.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BuildCtx {
    pub cluster: u64,
    pub now: FileTime,
    /// The volume is NTFS, where small streams can be resident in the MFT.
    pub ntfs: bool,
}

impl BuildCtx {
    /// Normalises a reported allocation to the MFT scanner's convention.
    ///
    /// NTFS reports a resident stream's allocation as its size rounded up to
    /// 8 bytes, but resident data lives inside the MFT record and occupies no
    /// clusters of its own, so it counts as 0. Non-resident
    /// allocations are always whole clusters, so anything else is resident.
    pub(crate) fn on_disk(self, reported: u64) -> u64 {
        if self.ntfs && self.cluster > 0 && !reported.is_multiple_of(self.cluster) {
            0
        } else {
            reported
        }
    }
}

/// Builds a record for one listed entry, consuming its name.
///
/// Directories carry no stream sizes here (their index overhead is filled
/// when the directory itself is opened). Files take the listing's allocation
/// when present; otherwise the allocation is estimated and flagged.
pub(crate) fn build(e: RawEntry, id: FileRef, parent: FileRef, cx: BuildCtx) -> ScanRecord {
    let mut flags = flags_for(e.attributes, e.reparse_tag, &e.times, cx.now);
    let sizes = if e.is_dir() {
        Sizes::default()
    } else {
        // NOTE: the WOF filter hides both the reparse attribute and the real
        // allocation of CompactOS / `compact /exe` files from directory
        // listings, which report 0. A non-empty file with 0 allocation that is
        // not sparse, compressed or remote can only be such a file, so its
        // allocation is unknown until the allocation pass opens it.
        let hidden = e.allocated == Some(0)
            && e.logical > 0
            && e.attributes
                & (win32::FILE_ATTRIBUTE_SPARSE_FILE
                    | win32::FILE_ATTRIBUTE_COMPRESSED
                    | RECALL_BITS)
                == 0;
        let allocated = match e.allocated {
            Some(a) if !hidden => cx.on_disk(a),
            _ => {
                flags |= EntryFlags::ALLOC_ESTIMATED;
                estimate_allocation(e.logical, cx.cluster, e.attributes)
            }
        };
        Sizes {
            logical: e.logical,
            allocated,
            ..Sizes::default()
        }
    };
    let reparse = (e.attributes & win32::FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(Reparse {
        tag: e.reparse_tag,
        target: None,
    });
    ScanRecord {
        id,
        links: vec![NameLink {
            parent,
            name: WideName::from_units(e.name),
        }],
        attributes: e.attributes,
        flags,
        times: e.times,
        fn_created: None,
        sizes,
        reparse,
        ads: Vec::new(),
    }
}

/// Records named streams on `rec`, folding `WofCompressedData` into the
/// unnamed stream's allocation for WOF files (it is the file's
/// real on-disk size and is not an ADS).
pub(crate) fn apply_streams(rec: &mut ScanRecord, streams: Vec<StreamEntry>, cx: BuildCtx) {
    let wof = rec.flags.reparse() == ReparseKind::Wof;
    let mut ads = Vec::with_capacity(streams.len());
    for s in streams {
        if is_wof_stream(&s.name) {
            if wof {
                rec.sizes.allocated = cx.on_disk(s.allocated);
            }
            continue;
        }
        ads.push(AdsInfo {
            name: WideName::from_units(s.name),
            logical: s.logical,
            allocated: cx.on_disk(s.allocated),
        });
    }
    rec.sizes.ads_logical = ads.iter().fold(0u64, |a, s| a.saturating_add(s.logical));
    rec.sizes.ads_allocated = ads.iter().fold(0u64, |a, s| a.saturating_add(s.allocated));
    rec.flags.set(EntryFlags::HAS_ADS, !ads.is_empty());
    rec.ads = ads;
}

#[cfg(test)]
mod tests {
    use super::*;

    const CX: BuildCtx = BuildCtx {
        cluster: 4096,
        now: FileTime(134_000_000_000_000_000),
        ntfs: true,
    };

    fn entry(name: &str, attrs: u32, tag: u32, logical: u64, alloc: Option<u64>) -> RawEntry {
        RawEntry {
            name: name.encode_utf16().collect(),
            attributes: attrs,
            reparse_tag: tag,
            times: Times {
                created: FileTime(133_000_000_000_000_000),
                modified: FileTime(133_000_000_000_000_000),
                ..Times::default()
            },
            logical,
            allocated: alloc,
            file_id: None,
        }
    }

    #[test]
    fn estimate_rounds_to_clusters_and_zeroes_remote_content() {
        assert_eq!(estimate_allocation(0, 4096, 0), 0);
        assert_eq!(estimate_allocation(1, 4096, 0), 4096);
        assert_eq!(estimate_allocation(4097, 4096, 0), 8192);
        assert_eq!(
            estimate_allocation(1 << 30, 4096, win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS),
            0
        );
        assert_eq!(estimate_allocation(u64::MAX, 4096, 0), u64::MAX);
    }

    #[test]
    fn missing_allocation_is_estimated_and_flagged() {
        let r = build(entry("a", 0x20, 0, 10, None), FileRef(1), FileRef(2), CX);
        assert_eq!(r.sizes.allocated, 4096);
        assert!(r.flags.contains(EntryFlags::ALLOC_ESTIMATED));
        let r = build(entry("a", 0x20, 0, 10, Some(8)), FileRef(1), FileRef(2), CX);
        assert_eq!(r.sizes.allocated, 0, "resident data occupies no clusters");
        assert!(!r.flags.contains(EntryFlags::ALLOC_ESTIMATED));
        let r = build(
            entry("a", 0x20, 0, 9000, Some(12_288)),
            FileRef(1),
            FileRef(2),
            CX,
        );
        assert_eq!(r.sizes.allocated, 12_288);
        assert!(!r.flags.contains(EntryFlags::ALLOC_ESTIMATED));
    }

    #[test]
    fn zero_allocation_of_nonempty_plain_file_is_treated_as_hidden_wof() {
        let r = build(
            entry("w", 0x20, 0, 100_000, Some(0)),
            FileRef(1),
            FileRef(2),
            CX,
        );
        assert!(r.flags.contains(EntryFlags::ALLOC_ESTIMATED));
        assert_eq!(r.sizes.allocated, 102_400);
        let sparse = build(
            entry("s", win32::FILE_ATTRIBUTE_SPARSE_FILE, 0, 100_000, Some(0)),
            FileRef(1),
            FileRef(2),
            CX,
        );
        assert!(!sparse.flags.contains(EntryFlags::ALLOC_ESTIMATED));
        assert_eq!(sparse.sizes.allocated, 0);
    }

    #[test]
    fn on_disk_only_rewrites_resident_sizes_on_ntfs() {
        assert_eq!(CX.on_disk(8), 0);
        assert_eq!(CX.on_disk(4096), 4096);
        assert_eq!(CX.on_disk(0), 0);
        let refs = BuildCtx { ntfs: false, ..CX };
        assert_eq!(refs.on_disk(8), 8);
    }

    #[test]
    fn cloud_and_reparse_classification() {
        let attrs =
            win32::FILE_ATTRIBUTE_REPARSE_POINT | win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
        let r = build(
            entry("c", attrs, 0x9000_701A, 1 << 20, None),
            FileRef(1),
            FileRef(2),
            CX,
        );
        assert_eq!(r.flags.reparse(), ReparseKind::Cloud);
        assert_eq!(r.flags.cloud(), CloudState::OnlineOnly);
        assert_eq!(r.sizes.allocated, 0);
        assert_eq!(r.reparse.as_ref().map(|p| p.tag), Some(0x9000_701A));
        let d = build(
            entry(
                "j",
                0x10 | win32::FILE_ATTRIBUTE_REPARSE_POINT,
                win32::IO_REPARSE_TAG_MOUNT_POINT,
                0,
                Some(0),
            ),
            FileRef(1),
            FileRef(2),
            CX,
        );
        assert!(d.is_dir());
        assert!(d.flags.reparse().blocks_traversal());
    }

    #[test]
    fn suspicious_times_are_flagged() {
        let mut e = entry("t", 0x20, 0, 0, Some(0));
        e.times.created = FileTime(0);
        let r = build(e, FileRef(1), FileRef(2), CX);
        assert!(r.flags.contains(EntryFlags::SUSPICIOUS_TIME));
    }

    #[test]
    fn wof_stream_becomes_allocation_not_ads() {
        let mut r = build(
            entry(
                "w",
                win32::FILE_ATTRIBUTE_REPARSE_POINT,
                win32::IO_REPARSE_TAG_WOF,
                100_000,
                Some(0),
            ),
            FileRef(1),
            FileRef(2),
            CX,
        );
        let s = |n: &str, l, a| StreamEntry {
            name: n.encode_utf16().collect(),
            logical: l,
            allocated: a,
        };
        apply_streams(
            &mut r,
            vec![s("WofCompressedData", 30_000, 32_768), s("x", 5, 8)],
            CX,
        );
        assert_eq!(r.sizes.allocated, 32_768);
        assert_eq!(r.ads.len(), 1);
        assert_eq!((r.sizes.ads_logical, r.sizes.ads_allocated), (5, 0));
        assert!(r.flags.contains(EntryFlags::HAS_ADS));
    }
}
