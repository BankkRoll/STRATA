//! Shared helpers for the integration tests.

#![allow(dead_code)]

use std::collections::BTreeMap;

use strata_core::{EntryFlags, FileRef, NameLink, ScanRecord, Sizes, WideName};
use strata_ntfs::test_image::DEFAULT_TIMES;
use strata_ntfs::{NtfsVolume, ScanOptions, ScanStats};

/// Scans an in-memory image with `opts`, returning records keyed by record number.
pub fn scan_with(img: Vec<u8>, opts: &ScanOptions) -> (BTreeMap<u64, ScanRecord>, ScanStats) {
    let vol = NtfsVolume::open(img).expect("image opens");
    let mut out = BTreeMap::new();
    let stats = vol
        .scan(opts, |batch| {
            for r in batch {
                assert!(
                    out.insert(r.id.record(), r).is_none(),
                    "record emitted twice"
                );
            }
        })
        .expect("scan succeeds");
    assert_eq!(stats.records_emitted as usize, out.len());
    (out, stats)
}

/// Scans with default options.
pub fn scan(img: Vec<u8>) -> (BTreeMap<u64, ScanRecord>, ScanStats) {
    scan_with(img, &ScanOptions::default())
}

/// A `(parent, name)` link.
pub fn link(parent: FileRef, name: &str) -> NameLink {
    NameLink {
        parent,
        name: WideName::from_str_lossless(name),
    }
}

/// The record the convenience builders produce, before case-specific changes.
pub fn base_record(id: FileRef, links: Vec<NameLink>, attributes: u32, sizes: Sizes) -> ScanRecord {
    ScanRecord {
        id,
        links,
        attributes,
        flags: EntryFlags::from_win32_attributes(attributes),
        times: DEFAULT_TIMES,
        fn_created: Some(DEFAULT_TIMES.created),
        sizes,
        reparse: None,
        ads: Vec::new(),
    }
}

/// Sizes of a file with only an unnamed stream.
pub fn sizes(logical: u64, allocated: u64) -> Sizes {
    Sizes {
        logical,
        allocated,
        ..Sizes::default()
    }
}
