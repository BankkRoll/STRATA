//! Cache file: round trips, header access, corruption detection.

mod common;

use common::*;
use strata_core::ScanRecord;
use strata_index::cache::{FORMAT_VERSION, read_header};
use strata_index::{CacheError, Index, IndexOptions, VolumeInfo};

fn records() -> Vec<ScanRecord> {
    let mut recs = vec![root_rec(), dir(r(30, 1), ROOT, "dir")];
    for i in 0..200u64 {
        let mut f = file(r(100 + i, 1), r(30, 1), &format!("file-{i}.bin"), i * 1000);
        if i % 50 == 0 {
            f.links.push(link(ROOT, &format!("hard-{i}")));
        }
        recs.push(f);
    }
    recs.push(file(r(400, 1), r(999, 1), "orphan.txt", 3));
    recs.push(record(
        r(401, 1),
        vec![link(ROOT, "huge")],
        false,
        6 << 40,
        6 << 40,
    ));
    recs
}

fn options(lite: bool) -> IndexOptions {
    IndexOptions {
        lite,
        volume: VolumeInfo {
            serial: 0xDEAD_BEEF,
            usn_journal_id: 77,
            last_usn: 123_456,
            prefix: "E:".into(),
        },
        ..opts()
    }
}

#[test]
fn round_trip_full_and_lite() {
    for lite in [false, true] {
        let mut idx = build(records(), options(lite));
        idx.upsert(file(r(500, 1), r(30, 1), "late", 9)).unwrap();
        idx.remove(r(101, 1)).unwrap();
        let bytes = idx.to_bytes();
        let back = Index::from_bytes(&bytes).unwrap();
        back.check_invariants().unwrap();
        assert_eq!(canonical(&back), canonical(&idx));
        assert_eq!(back.volume(), idx.volume());
        assert_eq!(back.options(), idx.options());
        assert_eq!(back.to_bytes(), bytes, "serialization is deterministic");
    }
}

#[test]
fn save_and_load_through_a_file() {
    let idx = build(records(), options(false));
    let dir = std::env::temp_dir().join(format!("strata-index-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("E.idx");
    idx.save(&path).unwrap();
    let back = Index::load(&path).unwrap();
    assert_eq!(canonical(&back), canonical(&idx));
    let h = read_header(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(h.version, FORMAT_VERSION);
    assert_eq!(
        (h.volume_serial, h.usn_journal_id, h.last_usn),
        (0xDEAD_BEEF, 77, 123_456)
    );
    assert_eq!(h.live, idx.len() as u64);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn loaded_index_accepts_live_updates() {
    let idx = build(records(), options(false));
    let mut back = Index::from_bytes(&idx.to_bytes()).unwrap();
    back.upsert(file(r(600, 1), r(30, 1), "after-load", 1))
        .unwrap();
    back.remove(r(102, 1)).unwrap();
    back.check_invariants().unwrap();
}

#[test]
fn every_flipped_byte_is_detected() {
    let bytes = build(records(), options(false)).to_bytes();
    // Flip one byte in each 97-byte stride: covers header, table and all
    // sections without making the test slow.
    for pos in (0..bytes.len()).step_by(97) {
        let mut bad = bytes.clone();
        bad[pos] ^= 0x55;
        match Index::from_bytes(&bad) {
            Err(_) => {}
            Ok(_) => {
                // Only padding between sections is not covered by a checksum.
                assert!(
                    is_padding(&bytes, pos),
                    "corruption at {pos} went unnoticed"
                );
            }
        }
    }
}

fn is_padding(bytes: &[u8], pos: usize) -> bool {
    let count = u32::from_le_bytes(bytes[92..96].try_into().unwrap()) as usize;
    let mut covered = 0..128 + count * 32;
    if covered.contains(&pos) {
        return false;
    }
    for i in 0..count {
        let e = 128 + i * 32;
        let off = u64::from_le_bytes(bytes[e + 8..e + 16].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(bytes[e + 16..e + 24].try_into().unwrap()) as usize;
        covered = off..off + len;
        if covered.contains(&pos) {
            return false;
        }
    }
    true
}

#[test]
fn truncation_and_garbage_fail_cleanly() {
    let bytes = build(records(), options(false)).to_bytes();
    for len in [0, 7, 100, 128, 200, bytes.len() / 2, bytes.len() - 1] {
        assert!(Index::from_bytes(&bytes[..len]).is_err(), "len {len}");
    }
    assert!(matches!(
        Index::from_bytes(b"not an index at all......"),
        Err(CacheError::BadMagic)
    ));
    let mut v = bytes.clone();
    v[8] = 99;
    assert!(matches!(
        Index::from_bytes(&v),
        Err(CacheError::UnsupportedVersion(99))
    ));
}
