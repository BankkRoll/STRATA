//! Runlist (mapping pairs) decoding, plus the encode/decode round trip.

#![no_main]

use libfuzzer_sys::fuzz_target;

use strata_ntfs::{decode_runlist, encode_runlist};
use strata_ntfs_fuzz as _;

fuzz_target!(|data: &[u8]| {
    if data.len() < 16 {
        return;
    }
    let start = u64::from_le_bytes(data[0..8].try_into().unwrap_or_default());
    let total = u64::from_le_bytes(data[8..16].try_into().unwrap_or_default());
    if let Ok(runs) = decode_runlist(&data[16..], start, total) {
        for r in &runs {
            assert!(r.len > 0);
            if let Some(l) = r.lcn {
                assert!(l.checked_add(r.len).is_some_and(|e| e <= total));
            }
        }
        // Valid runs re-encode to a list that decodes to the same runs.
        assert_eq!(
            decode_runlist(&encode_runlist(&runs), start, total).ok(),
            Some(runs)
        );
    }
});
