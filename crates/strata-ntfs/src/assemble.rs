//! Merging a base record and its extension records into a [`ScanRecord`].
//!
//! This is where SPEC §6.2 and §7 size rules are applied: VCN-0 sizes,
//! resident data, compressed/sparse total-allocated, WOF, cloud state,
//! directory index overhead, hardlinks and metadata flags.

use strata_core::{
    AdsInfo, CloudState, EntryFlags, FileRef, Reparse, ReparseKind, ScanRecord, Sizes, WideName,
    win32,
};

use crate::record::{DataPiece, ParsedRecord, ReparseLoc};
use crate::runlist::allocated_clusters;

/// Record number of `$Extend`; its children are NTFS metadata.
pub const EXTEND_RECORD: u64 = 11;
/// Record number of `$BadClus`.
pub const BADCLUS_RECORD: u64 = 8;
/// Records below this are reserved NTFS metadata (except the root, 5).
pub const FIRST_USER_RECORD: u64 = 16;

/// The named stream holding a WOF-compressed file's real bytes.
pub const WOF_STREAM: &str = "WofCompressedData";

/// Merges `base` with its extension records into one [`ScanRecord`].
///
/// `extensions` must already be filtered to records whose base reference
/// matches `base`; they are applied in record-number order so the output is
/// deterministic. `reparse` overrides the base's reparse value (used when a
/// non-resident reparse buffer was read from disk). `cluster_size` is needed
/// only for the `$BadClus:$Bad` rule.
///
/// # Example
///
/// ```
/// use strata_ntfs::{assemble, ParsedRecord};
/// let p = ParsedRecord {
///     record: 40, sequence: 3, link_count: 1, is_dir: false, base: None,
///     std_info: None, links: vec![], fn_created: None, data: vec![], index_allocations: vec![],
///     reparse: None, attr_list: None, bitmap: None, other_allocated: 0,
/// };
/// let r = assemble(p, Vec::new(), None, 4096);
/// assert_eq!(r.id.record(), 40);
/// assert!(r.links.is_empty());
/// ```
#[must_use]
pub fn assemble(
    mut base: ParsedRecord,
    mut extensions: Vec<ParsedRecord>,
    reparse: Option<Reparse>,
    cluster_size: u64,
) -> ScanRecord {
    extensions.sort_by_key(|e| e.record);
    let mut links = std::mem::take(&mut base.links);
    let mut fn_created = base.fn_created;
    let mut data = std::mem::take(&mut base.data);
    let mut index = std::mem::take(&mut base.index_allocations);
    let mut std_info = base.std_info;
    let mut reparse_loc = base.reparse.take();
    for mut e in extensions {
        links.append(&mut e.links);
        fn_created = fn_created.or(e.fn_created);
        data.append(&mut e.data);
        index.append(&mut e.index_allocations);
        std_info = std_info.or(e.std_info);
        if reparse_loc.is_none() {
            reparse_loc = e.reparse.take();
        }
    }
    let reparse = reparse.or(match reparse_loc {
        Some(ReparseLoc::Resident(r)) => Some(r),
        _ => None,
    });

    let std_info = std_info.unwrap_or_default();
    let mut attributes = std_info.attributes;
    if base.is_dir {
        attributes |= win32::FILE_ATTRIBUTE_DIRECTORY;
    }
    let kind = reparse
        .as_ref()
        .map_or(ReparseKind::None, |r| ReparseKind::from_tag(r.tag));
    let is_wof = kind == ReparseKind::Wof;

    let mut sizes = Sizes::default();
    let mut ads = Vec::new();
    let mut wof_allocated = None;
    // PERF: streams are grouped by scanning instead of building per-name
    // vectors; nearly every record has one or two pieces, and the scan avoids
    // two allocations per record on the hot path.
    for (i, first) in data.iter().enumerate() {
        let name = &first.name;
        if data[..i].iter().any(|q| &q.name == name) {
            continue;
        }
        let pieces = || data[i..].iter().filter(move |q| &q.name == name);
        let (logical, mut allocated) = pieces()
            .find(|p| p.start_vcn == 0)
            .map_or((0, 0), |p| (p.logical, p.allocated));
        if base.record == BADCLUS_RECORD && is_named(name, "$Bad") {
            // NOTE: $BadClus:$Bad spans the whole volume as sparse runs, but
            // its header does not always carry the sparse flag, so the header
            // allocation would claim the entire disk. Count real runs only.
            if let Some(clusters) = run_clusters(pieces()) {
                allocated = clusters.saturating_mul(cluster_size);
            }
        }
        if name.is_empty() {
            sizes.logical = logical;
            sizes.allocated = allocated;
        } else if is_wof && is_named(name, WOF_STREAM) {
            wof_allocated = Some(allocated);
        } else {
            sizes.ads_logical = sizes.ads_logical.saturating_add(logical);
            sizes.ads_allocated = sizes.ads_allocated.saturating_add(allocated);
            ads.push(AdsInfo {
                name: name.clone(),
                logical,
                allocated,
            });
        }
    }
    if let Some(a) = wof_allocated {
        sizes.allocated = a;
    }
    sizes.dir_overhead = index
        .iter()
        .filter(|p| p.start_vcn == 0)
        .fold(0u64, |acc, p| acc.saturating_add(p.allocated));

    let mut flags = EntryFlags::from_win32_attributes(attributes).with_reparse(kind);
    flags.set(EntryFlags::DIR, base.is_dir);
    flags.set(EntryFlags::HAS_ADS, !ads.is_empty());
    if kind == ReparseKind::Cloud {
        flags = flags.with_cloud(CloudState::from_attributes(attributes));
    }
    let is_metadata = (base.record < FIRST_USER_RECORD && base.record != FileRef::NTFS_ROOT_RECORD)
        || links.iter().any(|l| l.parent.record() == EXTEND_RECORD);
    flags.set(EntryFlags::NTFS_METADATA, is_metadata);

    ScanRecord {
        id: FileRef::from_parts(base.record, base.sequence),
        links,
        attributes,
        flags,
        times: std_info.times,
        fn_created,
        sizes,
        reparse,
        ads,
    }
}

fn is_named(name: &WideName, ascii: &str) -> bool {
    name.units().iter().copied().eq(ascii.encode_utf16())
}

/// Non-sparse clusters across all pieces, if every piece had its runs decoded.
fn run_clusters<'a>(mut pieces: impl Iterator<Item = &'a DataPiece>) -> Option<u64> {
    pieces.try_fold(0u64, |acc, p| {
        p.runs
            .as_deref()
            .map(|r| acc.saturating_add(allocated_clusters(r)))
    })
}
