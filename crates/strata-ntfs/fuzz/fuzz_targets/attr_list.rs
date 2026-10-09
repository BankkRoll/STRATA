//! `$ATTRIBUTE_LIST` and `$REPARSE_POINT` value decoding.

#![no_main]

use libfuzzer_sys::fuzz_target;

use strata_ntfs::{parse_attr_list, parse_file_name, parse_reparse, parse_std_info};
use strata_ntfs_fuzz as _;

fuzz_target!(|data: &[u8]| {
    let _ = parse_attr_list(data);
    let _ = parse_reparse(data);
    let _ = parse_file_name(data);
    let _ = parse_std_info(data);
});
