//! Volume-level behaviour: geometries, MFT fragmentation and bootstrap,
//! chunking, the `$MFT:$BITMAP` option, cancellation, I/O failure handling,
//! raw-volume read modes and `$Bitmap` reconciliation.

mod common;

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{base_record, link, scan, scan_with, sizes};
use strata_core::{EntryFlags, FileRef, ScanRecord, Sizes, win32};
use strata_ntfs::test_image::{
    Geometry, ImageBuilder, NonResidentSpec, ROOT, RecordBuilder, chain_runs, sparse_run,
};
use strata_ntfs::{IoMode, NtfsError, NtfsVolume, RawVolume, ReadAt, ScanOptions};

const ARCHIVE: u32 = win32::FILE_ATTRIBUTE_ARCHIVE;

/// Builds a volume with a fragmented MFT whose `$DATA` continues in an
/// extension record, plus a representative file set. Returns the image and
/// the expected records for the user files.
fn geometry_image(g: Geometry, list_nonresident: bool) -> (Vec<u8>, Vec<ScanRecord>) {
    let cs = u64::from(g.cluster_size);
    let mut b = ImageBuilder::new(g)
        .with_system_files()
        .mft_fragments(5)
        .mft_data_in_extension(15);
    if list_nonresident {
        b = b.mft_attr_list_nonresident();
    }
    let data = b.alloc(3);
    let index = b.alloc(1);
    let sparse_real = b.alloc(1);
    let sparse = chain_runs(0, &[sparse_real, sparse_run(9)]);
    let dir = FileRef::from_parts(64, 1);
    b.insert(
        64,
        RecordBuilder::dir(1, ROOT, "dir")
            .index_allocation("$I30", NonResidentSpec::new(index, cs, g.cluster_size)),
    );
    b.insert(
        65,
        RecordBuilder::file(1, dir, "resident.txt").data("", b"resident"),
    );
    b.insert(
        66,
        RecordBuilder::file(1, dir, "nonresident.bin").data_nonresident(
            "",
            0,
            NonResidentSpec::new(data, 2 * cs + 1, g.cluster_size),
        ),
    );
    b.insert(
        67,
        RecordBuilder::new(1)
            .std_info(
                strata_ntfs::test_image::DEFAULT_TIMES,
                ARCHIVE | win32::FILE_ATTRIBUTE_SPARSE_FILE,
            )
            .name(dir, "sparse.img", strata_ntfs::NS_WIN32_AND_DOS)
            .data_sparse(
                "",
                NonResidentSpec::new(sparse, 10 * cs, g.cluster_size),
                cs,
            ),
    );
    let mut d = base_record(
        dir,
        vec![link(ROOT, "dir")],
        win32::FILE_ATTRIBUTE_DIRECTORY,
        Sizes {
            dir_overhead: cs,
            ..Sizes::default()
        },
    );
    d.flags = EntryFlags::DIR;
    let want = vec![
        d,
        base_record(
            FileRef::from_parts(65, 1),
            vec![link(dir, "resident.txt")],
            ARCHIVE,
            sizes(8, 0),
        ),
        base_record(
            FileRef::from_parts(66, 1),
            vec![link(dir, "nonresident.bin")],
            ARCHIVE,
            sizes(2 * cs + 1, 3 * cs),
        ),
        base_record(
            FileRef::from_parts(67, 1),
            vec![link(dir, "sparse.img")],
            ARCHIVE | win32::FILE_ATTRIBUTE_SPARSE_FILE,
            sizes(10 * cs, cs),
        ),
    ];
    (b.finish(), want)
}

const GEOMETRIES: [Geometry; 9] = [
    Geometry {
        sector_size: 512,
        cluster_size: 512,
        record_size: 512,
    },
    Geometry {
        sector_size: 512,
        cluster_size: 512,
        record_size: 1024,
    },
    Geometry {
        sector_size: 512,
        cluster_size: 1024,
        record_size: 1024,
    },
    Geometry {
        sector_size: 512,
        cluster_size: 4096,
        record_size: 1024,
    },
    Geometry {
        sector_size: 512,
        cluster_size: 4096,
        record_size: 4096,
    },
    Geometry {
        sector_size: 4096,
        cluster_size: 4096,
        record_size: 4096,
    },
    Geometry {
        sector_size: 512,
        cluster_size: 65536,
        record_size: 1024,
    },
    Geometry {
        sector_size: 512,
        cluster_size: 2 * 1024 * 1024,
        record_size: 1024,
    },
    Geometry {
        sector_size: 4096,
        cluster_size: 2 * 1024 * 1024,
        record_size: 4096,
    },
];

#[test]
fn every_geometry_scans_identically_and_reconciles_exactly() {
    for (i, g) in GEOMETRIES.into_iter().enumerate() {
        let (image, want) = geometry_image(g, i % 2 == 0);
        let vol = NtfsVolume::open(image.clone()).unwrap();
        assert_eq!(vol.boot().cluster_size, u64::from(g.cluster_size), "{g:?}");
        assert_eq!(vol.boot().record_size, g.record_size, "{g:?}");
        assert!(
            vol.layout().fragment_count() >= 5,
            "{g:?}: {:?}",
            vol.layout().runs
        );
        let (recs, stats) = scan(image);
        for w in &want {
            assert_eq!(&recs[&w.id.record()], w, "{g:?}");
        }
        assert!(
            !recs.contains_key(&15),
            "$MFT extension record must be merged, {g:?}"
        );
        assert_eq!(stats.extensions_merged, 1, "{g:?}");
        assert_eq!(
            stats.corrupt + stats.torn + stats.malformed + stats.bad_signature,
            0
        );
        assert_eq!(stats.free + stats.in_use, stats.records_total, "{g:?}");

        let used = vol.count_used_clusters().unwrap() * vol.boot().cluster_size;
        let found: u64 = recs
            .values()
            .map(|r| r.sizes.total_allocated())
            .sum::<u64>()
            + stats.other_attr_allocated;
        assert_eq!(used, found, "volume used vs. sum allocated, {g:?}");
    }
}

#[test]
fn chunk_boundaries_and_straddling_records_do_not_change_results() {
    // 512-byte clusters with 1 KiB records and 7 MFT fragments: fragments
    // with an odd cluster count split records across extents.
    let g = Geometry {
        sector_size: 512,
        cluster_size: 512,
        record_size: 1024,
    };
    let mut b = ImageBuilder::new(g)
        .with_system_files()
        .mft_fragments(7)
        .min_records(205);
    for n in 16..200 {
        b.insert(
            n,
            RecordBuilder::file(1, ROOT, &format!("f{n}")).data("", &vec![1u8; n as usize]),
        );
    }
    let image = b.finish();
    let vol = NtfsVolume::open(image.clone()).unwrap();
    assert!(vol.layout().runs.iter().any(|r| r.len % 2 == 1));
    let (reference, _) = scan(image.clone());
    for chunk_records in [1usize, 3, 7, 64, 4096] {
        let opts = ScanOptions {
            chunk_bytes: chunk_records * 1024,
            queue_depth: 1,
            ..ScanOptions::default()
        };
        let (recs, _) = scan_with(image.clone(), &opts);
        assert_eq!(recs, reference, "chunk of {chunk_records} records");
    }
    for n in 16..200u64 {
        assert_eq!(reference[&n].sizes.logical, n);
    }
}

#[test]
fn mft_bitmap_option_skips_unused_records_with_identical_output() {
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .min_records(20_000);
    for n in [30u64, 31, 9000, 19_999] {
        b.insert(
            n,
            RecordBuilder::file(1, ROOT, &format!("f{n}")).data("", b"x"),
        );
    }
    let image = b.finish();
    let (full, full_stats) = scan(image.clone());
    let opts = ScanOptions {
        use_mft_bitmap: true,
        chunk_bytes: 64 * 1024,
        ..ScanOptions::default()
    };
    let (fast, fast_stats) = scan_with(image, &opts);
    assert_eq!(full, fast);
    assert!(fast_stats.skipped_by_bitmap > 15_000);
    assert!(fast_stats.bytes_read < full_stats.bytes_read / 4);
    assert_eq!(fast_stats.in_use, full_stats.in_use);
    assert_eq!(fast_stats.free, full_stats.free);
}

#[test]
fn cancellation_stops_early_with_partial_results() {
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .min_records(4096);
    for n in 16..4096 {
        b.insert(
            n,
            RecordBuilder::file(1, ROOT, &format!("f{n}")).data("", b"x"),
        );
    }
    let vol = NtfsVolume::open(b.finish()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let opts = ScanOptions {
        chunk_bytes: 64 * 1024,
        queue_depth: 1,
        cancel: Some(cancel.clone()),
        ..ScanOptions::default()
    };
    let mut got = 0;
    let stats = vol
        .scan(&opts, |batch| {
            got += batch.len();
            cancel.store(true, Ordering::Relaxed);
        })
        .unwrap();
    assert!(stats.cancelled);
    assert_eq!(stats.records_emitted as usize, got);
    assert!(got > 0 && got < 4096, "got {got}");
}

#[test]
fn invalid_chunk_sizes_are_rejected() {
    let vol = NtfsVolume::open(ImageBuilder::new(Geometry::default()).finish()).unwrap();
    for chunk_bytes in [0, 1000, 1536] {
        let opts = ScanOptions {
            chunk_bytes,
            ..ScanOptions::default()
        };
        assert!(matches!(
            vol.scan(&opts, |_| {}),
            Err(NtfsError::InvalidOption(_))
        ));
    }
}

/// Fails every read that touches `bad`.
struct FlakyDisk {
    inner: Vec<u8>,
    bad: std::ops::Range<u64>,
}

impl ReadAt for FlakyDisk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset + buf.len() as u64;
        if offset < self.bad.end && end > self.bad.start {
            return Err(io::Error::other("bad sector"));
        }
        self.inner.read_at(offset, buf)
    }
}

#[test]
fn unreadable_sectors_cost_only_their_records() {
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .min_records(64);
    for n in 16..64 {
        b.insert(
            n,
            RecordBuilder::file(1, ROOT, &format!("f{n}")).data("", b"x"),
        );
    }
    let image = b.finish();
    let vol = NtfsVolume::open(image.clone()).unwrap();
    let rec40 = vol.layout().runs[0].lcn.unwrap() * 4096 + 40 * 1024;
    let flaky = FlakyDisk {
        inner: image,
        bad: rec40..rec40 + 512,
    };
    let vol = NtfsVolume::open(flaky).unwrap();
    let mut recs = BTreeMap::new();
    let stats = vol
        .scan(&ScanOptions::default(), |batch| {
            for r in batch {
                recs.insert(r.id.record(), r);
            }
        })
        .unwrap();
    assert_eq!(stats.unreadable, 1);
    assert!(!recs.contains_key(&40));
    assert!(recs.contains_key(&39) && recs.contains_key(&41));
}

#[test]
fn read_record_matches_scan_for_every_record() {
    let (image, _) = geometry_image(Geometry::default(), true);
    let vol = NtfsVolume::open(image.clone()).unwrap();
    let (recs, _) = scan(image);
    for n in 0..vol.layout().readable_records() {
        assert_eq!(
            vol.read_record(n).unwrap().as_ref(),
            recs.get(&n),
            "record {n}"
        );
    }
    assert!(matches!(
        vol.read_record(1 << 40),
        Err(NtfsError::OutOfRange(_))
    ));
}

#[test]
fn raw_volume_modes_over_an_image_file_match_memory() {
    let (image, _) = geometry_image(Geometry::default(), false);
    let (want, _) = scan(image.clone());
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("strata-ntfs-raw-modes.img");
    std::fs::write(&path, &image).unwrap();
    for mode in [IoMode::Sequential, IoMode::NoBuffering] {
        // A plain file open with a 4 KiB alignment exercises the same aligned
        // and bounce-buffered paths that FILE_FLAG_NO_BUFFERING requires.
        let file = std::fs::File::open(&path).unwrap();
        let vol = NtfsVolume::open(RawVolume::from_file(file, mode, 4096)).unwrap();
        let mut got = BTreeMap::new();
        vol.scan(&ScanOptions::default(), |b| {
            for r in b {
                got.insert(r.id.record(), r);
            }
        })
        .unwrap();
        assert_eq!(got, want, "{mode:?}");
    }
    // The real flag on an ordinary file: sector-aligned reads succeed.
    let vol = NtfsVolume::open(RawVolume::open(&path, IoMode::NoBuffering).unwrap()).unwrap();
    assert_eq!(vol.read_record(65).unwrap().as_ref(), want.get(&65));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn open_rejects_bad_boot_and_bad_mft() {
    assert!(matches!(
        NtfsVolume::open(vec![0u8; 4096]),
        Err(NtfsError::Boot(_))
    ));
    assert!(NtfsVolume::open(vec![0u8; 10]).is_err());

    // Record 0 marked BAAD.
    let image = ImageBuilder::new(Geometry::default()).finish();
    let vol = NtfsVolume::open(image.clone()).unwrap();
    let off = vol.boot().mft_offset() as usize;
    let mut broken = image.clone();
    broken[off..off + 4].copy_from_slice(b"BAAD");
    assert!(matches!(NtfsVolume::open(broken), Err(NtfsError::Mft(_))));

    // $MFT extension record wiped: its $DATA continuation cannot be found.
    let image = ImageBuilder::new(Geometry::default())
        .mft_data_in_extension(15)
        .finish();
    let vol = NtfsVolume::open(image.clone()).unwrap();
    let off = vol.boot().mft_offset() as usize + 15 * 1024;
    let mut broken = image;
    broken[off..off + 1024].fill(0);
    assert!(matches!(NtfsVolume::open(broken), Err(NtfsError::Mft(_))));
}

#[test]
fn mft_with_many_fragments() {
    let mut b = ImageBuilder::new(Geometry::default())
        .mft_fragments(120)
        .min_records(4000)
        .mft_data_in_extension(3);
    for n in [16u64, 1999, 3999] {
        b.insert(n, RecordBuilder::file(1, ROOT, "x").data("", b"y"));
    }
    let image = b.finish();
    let vol = NtfsVolume::open(image.clone()).unwrap();
    assert_eq!(vol.layout().fragment_count(), 120);
    let (recs, _) = scan(image);
    for n in [16u64, 1999, 3999] {
        assert_eq!(recs[&n].sizes.logical, 1);
    }
}
