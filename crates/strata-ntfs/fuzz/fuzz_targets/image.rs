//! Whole images: boot sector, `$MFT` bootstrap (attribute list, extension
//! records), the scan pipeline, single-record reads and `$Bitmap` counting.

#![no_main]

use libfuzzer_sys::fuzz_target;

use strata_ntfs::{NtfsVolume, ScanOptions};
use strata_ntfs_fuzz as _;

fuzz_target!(|data: &[u8]| {
    let Ok(volume) = NtfsVolume::open(data) else {
        return;
    };
    let opts = ScanOptions {
        chunk_bytes: 64 * 1024,
        use_mft_bitmap: data.len() % 2 == 0,
        ..ScanOptions::default()
    };
    let _ = volume.scan(&opts, |_| {});
    for n in 0..32 {
        let _ = volume.read_record(n);
    }
    let _ = volume.count_used_clusters();
});
