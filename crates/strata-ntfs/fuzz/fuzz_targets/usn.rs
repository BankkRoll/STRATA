//! `FSCTL_READ_USN_JOURNAL` output buffers (USN_RECORD_V2/V3/V4).

#![no_main]

use libfuzzer_sys::fuzz_target;

use strata_ntfs::parse_usn_buffer;
use strata_ntfs_fuzz as _;

fuzz_target!(|data: &[u8]| {
    if let Ok((_, records)) = parse_usn_buffer(data) {
        for r in records {
            let _ = r;
        }
    }
});
