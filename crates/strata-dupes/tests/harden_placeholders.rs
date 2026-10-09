//! Real OneDrive attribute combinations: whatever state a cloud file is in
//! (online-only, locally available, pinned, a recall-on-open folder), it is
//! excluded before anything is opened when the index knows, and refused by
//! the handle gate when only the open handle shows it.

use strata_clean::CancelToken;
use strata_core::win32::{
    FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_PINNED, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_UNPINNED,
};
use strata_core::{CloudState, EntryFlags, FileRef, FileTime, ReparseKind};
use strata_dupes::*;

/// `IO_REPARSE_TAG_CLOUD_6` as OneDrive sets it.
const ONEDRIVE_TAG: u32 = 0x9000_701A;

const ONLINE_ONLY: u32 = FILE_ATTRIBUTE_UNPINNED
    | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    | FILE_ATTRIBUTE_OFFLINE
    | FILE_ATTRIBUTE_REPARSE_POINT
    | FILE_ATTRIBUTE_ARCHIVE;
const LOCALLY_AVAILABLE: u32 = FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_ARCHIVE;
const PINNED: u32 = FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_REPARSE_POINT;
const FOLDER: u32 = FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_DIRECTORY;

const COMBOS: [(&str, u32, u32); 4] = [
    ("online-only", ONLINE_ONLY, ONEDRIVE_TAG),
    ("locally-available", LOCALLY_AVAILABLE, ONEDRIVE_TAG),
    ("pinned", PINNED, ONEDRIVE_TAG),
    ("folder", FOLDER, 0),
];

/// Index flags exactly as the scanner and index derive them.
fn index_flags(attrs: u32, tag: u32) -> EntryFlags {
    let mut f = EntryFlags::from_win32_attributes(attrs);
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        f = f.with_reparse(ReparseKind::from_tag(tag));
    }
    if f.reparse() == ReparseKind::Cloud {
        f = f.with_cloud(CloudState::from_attributes(attrs));
    }
    f
}

#[test]
fn handle_gate_refuses_every_onedrive_state() {
    for (name, attrs, tag) in COMBOS {
        for allow_wof in [true, false] {
            assert!(
                matches!(
                    check_handle_attributes(attrs, tag, allow_wof),
                    Err(SkipReason::Placeholder { .. })
                ),
                "{name}"
            );
        }
    }
}

#[test]
fn index_known_placeholders_are_never_opened() {
    // The paths do not exist: opening any of them would surface as a
    // "not found" skip, so an empty skip list proves nothing was opened.
    let missing = std::env::temp_dir().join("strata-harden-never-created");
    let mut cands = Vec::new();
    for (i, (name, attrs, tag)) in COMBOS.iter().enumerate() {
        for copy in 0..2 {
            cands.push(Candidate {
                volume: VolumeKey::new(0xD, "v"),
                file_ref: FileRef::from_parts(100 + 2 * i as u64 + copy, 1),
                path: missing.join(format!("{name}-{copy}.bin")),
                size: 8 << 20,
                mtime: FileTime(133_000_000_000_000_000),
                flags: index_flags(*attrs, *tag),
            });
        }
    }
    let cfg = ScanConfig {
        min_size: 1,
        ..ScanConfig::default()
    };
    let r = match find_duplicates(
        cands,
        &cfg,
        &MemoryHashCache::new(),
        &CancelToken::new(),
        &|_| {},
    ) {
        ScanOutcome::Completed(r) => r,
        ScanOutcome::Cancelled(_) => panic!("cancelled"),
    };
    assert!(r.groups.is_empty());
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    assert_eq!(r.stats.measured, 0);
    assert_eq!(r.stats.bytes_read, 0);
    assert_eq!(r.stats.excluded[&Exclusion::CloudPlaceholder], 6);
    assert_eq!(r.stats.excluded[&Exclusion::Directory], 2);
}
