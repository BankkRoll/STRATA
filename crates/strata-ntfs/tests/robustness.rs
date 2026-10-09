//! Stable "never panics" property tests (SPEC §6.2, §22).
//!
//! These mirror the cargo-fuzz targets in `fuzz/` so the guarantee is
//! checked on every `cargo test`, without a nightly toolchain: arbitrary
//! bytes and arbitrary mutations of valid records, runlists, attribute
//! lists, USN buffers, boot sectors and whole images.

use proptest::prelude::*;
use strata_ntfs::test_image::{Geometry, sample_image, sample_records};
use strata_ntfs::{
    AttrIter, BootSector, NtfsVolume, ParseOptions, RecordOutcome, ScanOptions, assemble,
    decode_runlist, parse_attr_list, parse_record, parse_reparse, parse_usn_buffer,
};

/// Case count: `PROPTEST_CASES` when set (for long local runs), else `default`.
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn valid_records() -> Vec<Vec<u8>> {
    let g = Geometry::default();
    sample_records()
        .into_iter()
        .map(|(n, r)| r.build(g, n))
        .collect()
}

/// Parses like the scanner does, including assembly, so the whole
/// per-record path is exercised.
fn exercise_record(mut rec: Vec<u8>, decode_runs: bool) {
    let opts = ParseOptions {
        total_clusters: 1 << 20,
        decode_runs,
        capture_bitmap: true,
    };
    if let RecordOutcome::InUse(p) = parse_record(&mut rec, 70, &opts) {
        let _ = assemble(*p, Vec::new(), None, 4096);
    }
    for a in AttrIter::new(&rec, 0x38, rec.len()) {
        let _ = a;
    }
}

/// Byte-level mutations: overwrite `edits` positions with chosen values.
fn mutate(mut data: Vec<u8>, edits: &[(usize, u8)]) -> Vec<u8> {
    let len = data.len();
    for &(pos, val) in edits {
        if len > 0 {
            data[pos % len] = val;
        }
    }
    data
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(512), ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_as_records(bytes in proptest::collection::vec(any::<u8>(), 0..4200), runs in any::<bool>()) {
        exercise_record(bytes, runs);
    }

    #[test]
    fn arbitrary_bytes_with_file_header(mut bytes in proptest::collection::vec(any::<u8>(), 1024), usa in 0u16..0x200) {
        bytes[0..4].copy_from_slice(b"FILE");
        bytes[4..6].copy_from_slice(&usa.to_le_bytes());
        bytes[6..8].copy_from_slice(&3u16.to_le_bytes());
        bytes[0x16] |= 1;
        exercise_record(bytes, true);
    }

    #[test]
    fn mutated_valid_records(idx in 0usize..15, edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..16), runs in any::<bool>()) {
        let recs = valid_records();
        let rec = recs[idx % recs.len()].clone();
        exercise_record(mutate(rec, &edits), runs);
    }

    #[test]
    fn arbitrary_runlists(bytes in proptest::collection::vec(any::<u8>(), 0..512), start in any::<u64>(), total in any::<u64>()) {
        if let Ok(runs) = decode_runlist(&bytes, start, total) {
            for r in runs {
                prop_assert!(r.len > 0);
                if let Some(l) = r.lcn {
                    prop_assert!(l.checked_add(r.len).is_some_and(|e| e <= total));
                }
            }
        }
    }

    #[test]
    fn arbitrary_attribute_lists(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = parse_attr_list(&bytes);
    }

    #[test]
    fn arbitrary_reparse_buffers(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = parse_reparse(&bytes);
    }

    #[test]
    fn arbitrary_usn_buffers(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        if let Ok((_, it)) = parse_usn_buffer(&bytes) {
            for r in it.take(10_000) {
                let _ = r;
            }
        }
    }

    #[test]
    fn arbitrary_boot_sectors(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
        let _ = BootSector::parse(&bytes);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(48), ..ProptestConfig::default() })]

    /// Corrupting the MFT region of a full image: open + scan + read_record
    /// must return errors or counted skips, never panic.
    #[test]
    fn mutated_images_never_panic(edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..64), in_mft in any::<bool>()) {
        let image = sample_image();
        let vol = NtfsVolume::open(image.clone()).unwrap();
        let mft_start = vol.boot().mft_offset() as usize;
        let span = if in_mft { 128 * 1024 } else { image.len() };
        let base = if in_mft { mft_start.saturating_sub(64 * 1024) } else { 0 };
        let mut broken = image;
        let len = broken.len();
        for (pos, val) in edits {
            broken[(base + pos % span) % len] = val;
        }
        if let Ok(vol) = NtfsVolume::open(broken) {
            let opts = ScanOptions { chunk_bytes: 16 * 1024, use_mft_bitmap: true, ..ScanOptions::default() };
            let _ = vol.scan(&opts, |_| {});
            let _ = vol.scan(&ScanOptions::default(), |_| {});
            for n in 0..100 {
                let _ = vol.read_record(n);
            }
            let _ = vol.count_used_clusters();
        }
    }
}
