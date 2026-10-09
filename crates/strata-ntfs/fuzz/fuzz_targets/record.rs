//! MFT record parsing: fixups, header, attribute walk, value decoders and
//! assembly into a `ScanRecord`.

#![no_main]

use libfuzzer_sys::fuzz_target;

use strata_ntfs::{AttrIter, ParseOptions, RecordOutcome, assemble, parse_record};
use strata_ntfs_fuzz as _;

fuzz_target!(|data: &[u8]| {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let opts = ParseOptions {
        total_clusters: 1 << 32,
        decode_runs: flags & 1 != 0,
        capture_bitmap: flags & 2 != 0,
    };
    let mut rec = rest.to_vec();
    if let RecordOutcome::InUse(p) = parse_record(&mut rec, 42, &opts) {
        let _ = assemble(*p, Vec::new(), None, 4096);
    }
    for attr in AttrIter::new(rest, 0x38, rest.len()) {
        let _ = attr;
    }
});
