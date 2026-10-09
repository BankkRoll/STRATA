//! Writes the cargo-fuzz seed corpus from the synthetic image builder.
//!
//! ```text
//! cargo run -p strata-ntfs --features test-image --example gen_fuzz_corpus
//! ```
//!
//! Output: `crates/strata-ntfs/fuzz/corpus/<target>/seed-*`.

use std::fs;
use std::path::{Path, PathBuf};

use strata_core::{FileRef, FileTime, WideName};
use strata_ntfs::test_image::{
    AttrListSpec, DEFAULT_TIMES, Geometry, ImageBuilder, attr_list_value, file_name_value,
    mount_point_reparse, raw_reparse, sample_image, sample_records, std_info_value,
    symlink_reparse,
};
use strata_ntfs::usn::{FileId128, UsnChange, UsnExtent, UsnRange, encode};
use strata_ntfs::{AT_DATA, AT_FILE_NAME, NS_WIN32, Run, encode_runlist};

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    fs::create_dir_all(dir).expect("create corpus dir");
    fs::write(dir.join(name), bytes).expect("write seed");
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz")
        .join("corpus");

    let dir = root.join("record");
    for g in [
        Geometry::default(),
        Geometry {
            record_size: 4096,
            ..Geometry::default()
        },
    ] {
        for (n, r) in sample_records() {
            let mut seed = vec![1u8];
            seed.extend_from_slice(&r.build(g, n));
            write(&dir, &format!("seed-{}-{n}", g.record_size), &seed);
        }
    }

    let dir = root.join("runlist");
    let lists: [&[Run]; 4] = [
        &[Run {
            vcn: 0,
            lcn: Some(1000),
            len: 8,
        }],
        &[
            Run {
                vcn: 0,
                lcn: Some(5000),
                len: 3,
            },
            Run {
                vcn: 3,
                lcn: None,
                len: 100,
            },
            Run {
                vcn: 103,
                lcn: Some(10),
                len: 0x1_0000,
            },
        ],
        &[Run {
            vcn: 0,
            lcn: None,
            len: 1 << 40,
        }],
        &[
            Run {
                vcn: 0,
                lcn: Some(1 << 33),
                len: 1,
            },
            Run {
                vcn: 1,
                lcn: Some(1),
                len: 1,
            },
            Run {
                vcn: 2,
                lcn: Some(1 << 33),
                len: 0x7F,
            },
        ],
    ];
    for (i, runs) in lists.iter().enumerate() {
        let mut seed = 0u64.to_le_bytes().to_vec();
        seed.extend_from_slice(&(1u64 << 41).to_le_bytes());
        seed.extend_from_slice(&encode_runlist(runs));
        write(&dir, &format!("seed-{i}"), &seed);
    }

    let dir = root.join("attr_list");
    let base = FileRef::from_parts(40, 3);
    let name: Vec<u16> = "Zone.Identifier".encode_utf16().collect();
    let values = [
        attr_list_value(&[
            AttrListSpec::new(AT_FILE_NAME, 0, base),
            AttrListSpec::new(AT_DATA, 0, FileRef::from_parts(41, 1)),
            AttrListSpec {
                name: name.clone(),
                ..AttrListSpec::new(AT_DATA, 7, FileRef::from_parts(42, 1))
            },
        ]),
        symlink_reparse(r"\??\C:\target", r"C:\target", false),
        mount_point_reparse(r"\??\Volume{00000000-0000-0000-0000-000000000000}\", ""),
        raw_reparse(0x9000_601A, &[0; 24]),
        file_name_value(base, &name, NS_WIN32, DEFAULT_TIMES),
        std_info_value(DEFAULT_TIMES, 0x20),
    ];
    for (i, v) in values.iter().enumerate() {
        write(&dir, &format!("seed-{i}"), v);
    }

    let dir = root.join("usn");
    let change = |major| UsnChange {
        major_version: major,
        file: FileId128(0x0002_0000_0000_1234),
        parent: FileId128(0x0005_0000_0000_0005),
        usn: 0x1000,
        timestamp: FileTime(133_000_000_000_000_000),
        reason: 0x8000_0100,
        source_info: 0,
        security_id: 0x101,
        attributes: 0x20,
        name: WideName::from_str_lossless("new file.txt"),
    };
    let range = UsnRange {
        file: FileId128(77),
        parent: FileId128(5),
        usn: 0x2000,
        reason: 1,
        source_info: 0,
        remaining_extents: 1,
        extents: vec![UsnExtent {
            offset: 0,
            length: 4096,
        }],
    };
    write(
        &dir,
        "seed-v2",
        &encode::buffer(0x3000, &[encode::change(&change(2))]),
    );
    write(
        &dir,
        "seed-mixed",
        &encode::buffer(
            0x3000,
            &[
                encode::change(&change(2)),
                encode::change(&change(3)),
                encode::range(&range),
            ],
        ),
    );

    let dir = root.join("image");
    write(
        &dir,
        "seed-minimal",
        &ImageBuilder::new(Geometry::default()).finish(),
    );
    write(&dir, "seed-sample", &sample_image());
    println!("corpus written to {}", root.display());
}
