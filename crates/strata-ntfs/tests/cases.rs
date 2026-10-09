//! One test per NTFS edge case, each asserting the exact
//! `ScanRecord`s the scanner emits for a synthetic image.

mod common;

use common::{base_record, link, scan, sizes};
use strata_core::{
    AdsInfo, CloudState, EntryFlags, FileRef, NameLink, Reparse, ReparseKind, ScanRecord, Sizes,
    Times, WideName, win32,
};
use strata_ntfs::test_image::{
    AttrListSpec, AttrValue, DEFAULT_TIMES, EXTEND, Geometry, ImageBuilder, NonResidentSpec, ROOT,
    RecordBuilder, attr_list_value, chain_runs, mount_point_reparse, raw_reparse, sparse_run,
    symlink_reparse,
};
use strata_ntfs::{
    AT_ATTRIBUTE_LIST, AT_DATA, AT_FILE_NAME, AT_STANDARD_INFORMATION, NS_DOS, NS_POSIX, NS_WIN32,
    NS_WIN32_AND_DOS, NtfsVolume,
};

const ARCHIVE: u32 = win32::FILE_ATTRIBUTE_ARCHIVE;
const CS: u64 = 4096;

fn img() -> ImageBuilder {
    ImageBuilder::new(Geometry::default()).with_system_files()
}

fn fref(n: u64) -> FileRef {
    FileRef::from_parts(n, 1)
}

#[test]
fn resident_and_nonresident_unnamed_data() {
    let mut b = img();
    let runs = b.alloc(3);
    b.insert(
        64,
        RecordBuilder::file(1, ROOT, "small.txt").data("", b"hello"),
    );
    b.insert(
        65,
        RecordBuilder::file(2, ROOT, "big.bin").data_nonresident(
            "",
            0,
            NonResidentSpec::new(runs, 10_000, 4096),
        ),
    );
    b.insert(66, RecordBuilder::file(1, ROOT, "empty.txt").data("", &[]));
    let (recs, _) = scan(b.finish());
    assert_eq!(
        recs[&64],
        base_record(
            fref(64),
            vec![link(ROOT, "small.txt")],
            ARCHIVE,
            sizes(5, 0)
        )
    );
    assert_eq!(
        recs[&65],
        base_record(
            FileRef::from_parts(65, 2),
            vec![link(ROOT, "big.bin")],
            ARCHIVE,
            sizes(10_000, 3 * CS)
        )
    );
    assert_eq!(
        recs[&66],
        base_record(
            fref(66),
            vec![link(ROOT, "empty.txt")],
            ARCHIVE,
            sizes(0, 0)
        )
    );
}

#[test]
fn hardlinks_skip_dos_names_and_keep_first_fn_created() {
    let dir_a = fref(70);
    let dir_b = fref(71);
    let fn_times = Times {
        created: strata_core::FileTime(42),
        ..DEFAULT_TIMES
    };
    let mut b = img();
    b.insert(70, RecordBuilder::dir(1, ROOT, "A"));
    b.insert(71, RecordBuilder::dir(1, ROOT, "B"));
    let long: Vec<u16> = "Long File Name.txt".encode_utf16().collect();
    b.insert(
        72,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, ARCHIVE)
            .name_units(dir_a, &long, NS_WIN32, fn_times)
            .name(dir_a, "LONGFI~1.TXT", NS_DOS)
            .name(dir_b, "second link.txt", NS_WIN32_AND_DOS)
            .name(dir_b, "posix-link", NS_POSIX)
            .data("", b"x"),
    );
    let (recs, _) = scan(b.finish());
    let mut want = base_record(
        fref(72),
        vec![
            link(dir_a, "Long File Name.txt"),
            link(dir_b, "second link.txt"),
            link(dir_b, "posix-link"),
        ],
        ARCHIVE,
        sizes(1, 0),
    );
    want.fn_created = Some(strata_core::FileTime(42));
    assert_eq!(recs[&72], want);
    let mut dir = base_record(
        dir_a,
        vec![link(ROOT, "A")],
        win32::FILE_ATTRIBUTE_DIRECTORY,
        sizes(0, 0),
    );
    dir.flags = EntryFlags::DIR;
    assert_eq!(recs[&70], dir);
}

#[test]
fn posix_case_variants_and_unpaired_surrogates_are_kept_exactly() {
    let dir = fref(80);
    let mut b = img();
    b.insert(80, RecordBuilder::dir(1, ROOT, "wsl").directory());
    b.insert(
        81,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, ARCHIVE)
            .name(dir, "A.txt", NS_POSIX)
            .data("", b"1"),
    );
    b.insert(
        82,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, ARCHIVE)
            .name(dir, "a.txt", NS_POSIX)
            .data("", b"22"),
    );
    let lone = [u16::from(b'x'), 0xDC00, u16::from(b'y'), 0xD83D];
    b.insert(
        83,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, ARCHIVE)
            .name_units(dir, &lone, NS_POSIX, DEFAULT_TIMES)
            .data("", &[]),
    );
    let (recs, _) = scan(b.finish());
    assert_eq!(
        recs[&81],
        base_record(fref(81), vec![link(dir, "A.txt")], ARCHIVE, sizes(1, 0))
    );
    assert_eq!(
        recs[&82],
        base_record(fref(82), vec![link(dir, "a.txt")], ARCHIVE, sizes(2, 0))
    );
    let want = base_record(
        fref(83),
        vec![NameLink {
            parent: dir,
            name: WideName::from_units(lone.to_vec()),
        }],
        ARCHIVE,
        sizes(0, 0),
    );
    assert_eq!(recs[&83], want);
    assert!(recs[&83].links[0].name.has_unpaired_surrogate());
}

#[test]
fn sparse_and_compressed_use_total_allocated() {
    let mut b = img().claim_clusters(1 << 22);
    let real = b.alloc(256);
    let sparse_runs = chain_runs(0, &[real.clone(), sparse_run((1 << 20) - 256)]);
    let comp = b.alloc(10);
    let comp_runs = chain_runs(0, &[comp, sparse_run(6)]);
    let sparse_attrs = ARCHIVE | win32::FILE_ATTRIBUTE_SPARSE_FILE;
    let comp_attrs = ARCHIVE | win32::FILE_ATTRIBUTE_COMPRESSED;
    b.insert(
        90,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, sparse_attrs)
            .name(ROOT, "sparse.vhd", NS_WIN32_AND_DOS)
            .data_sparse(
                "",
                NonResidentSpec::new(sparse_runs, 4 << 30, 4096),
                256 * CS,
            ),
    );
    b.insert(
        91,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, comp_attrs)
            .name(ROOT, "packed.log", NS_WIN32_AND_DOS)
            .data_compressed("", NonResidentSpec::new(comp_runs, 60_000, 4096), 10 * CS),
    );
    let (recs, _) = scan(b.finish());
    assert_eq!(
        recs[&90],
        base_record(
            fref(90),
            vec![link(ROOT, "sparse.vhd")],
            sparse_attrs,
            sizes(4 << 30, 256 * CS)
        )
    );
    assert!(recs[&90].flags.contains(EntryFlags::SPARSE));
    assert_eq!(
        recs[&91],
        base_record(
            fref(91),
            vec![link(ROOT, "packed.log")],
            comp_attrs,
            sizes(60_000, 10 * CS)
        )
    );
    assert!(recs[&91].flags.contains(EntryFlags::COMPRESSED));
}

#[test]
fn wof_stream_becomes_allocated_and_is_not_an_ads() {
    let mut b = img();
    let wof = b.alloc(5);
    let wof_tag = raw_reparse(
        win32::IO_REPARSE_TAG_WOF,
        &[1, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
    );
    let attrs = ARCHIVE | win32::FILE_ATTRIBUTE_SPARSE_FILE | win32::FILE_ATTRIBUTE_REPARSE_POINT;
    b.insert(
        100,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, attrs)
            .name(ROOT, "notepad.exe", NS_WIN32_AND_DOS)
            .data_sparse("", NonResidentSpec::new(sparse_run(50), 200_000, 4096), 0)
            .data_nonresident(
                "WofCompressedData",
                0,
                NonResidentSpec::new(wof, 19_000, 4096),
            )
            .data("Zone.Identifier", &[0u8; 26])
            .reparse(wof_tag),
    );
    let (recs, _) = scan(b.finish());
    let mut want = base_record(
        fref(100),
        vec![link(ROOT, "notepad.exe")],
        attrs,
        Sizes {
            logical: 200_000,
            allocated: 5 * CS,
            ads_logical: 26,
            ads_allocated: 0,
            dir_overhead: 0,
            attr_overhead: 0,
        },
    );
    want.flags = want.flags.with_reparse(ReparseKind::Wof) | EntryFlags::HAS_ADS;
    want.reparse = Some(Reparse {
        tag: win32::IO_REPARSE_TAG_WOF,
        target: None,
    });
    want.ads = vec![AdsInfo {
        name: WideName::from_str_lossless("Zone.Identifier"),
        logical: 26,
        allocated: 0,
    }];
    assert_eq!(recs[&100], want);

    // Without the WOF tag the same stream is an ordinary ADS.
    let mut b = img();
    let runs = b.alloc(5);
    b.insert(
        101,
        RecordBuilder::file(1, ROOT, "fake.exe")
            .data("", b"abc")
            .data_nonresident(
                "WofCompressedData",
                0,
                NonResidentSpec::new(runs, 19_000, 4096),
            ),
    );
    let (recs, _) = scan(b.finish());
    assert_eq!(
        recs[&101].sizes,
        Sizes {
            logical: 3,
            allocated: 0,
            ads_logical: 19_000,
            ads_allocated: 5 * CS,
            dir_overhead: 0,
            attr_overhead: 0,
        }
    );
    assert!(recs[&101].flags.contains(EntryFlags::HAS_ADS));
}

#[test]
fn cloud_placeholders_map_pinned_unpinned_and_recall_bits() {
    const ONEDRIVE: u32 = 0x9000_601A;
    let cases = [
        (110, win32::FILE_ATTRIBUTE_PINNED, CloudState::AlwaysKeep),
        (
            111,
            win32::FILE_ATTRIBUTE_UNPINNED
                | win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
                | win32::FILE_ATTRIBUTE_OFFLINE,
            CloudState::OnlineOnly,
        ),
        (112, 0, CloudState::LocallyAvailable),
        (
            113,
            win32::FILE_ATTRIBUTE_RECALL_ON_OPEN,
            CloudState::OnlineOnly,
        ),
    ];
    let mut b = img();
    for (n, extra, _) in cases {
        b.insert(
            n,
            RecordBuilder::new(1)
                .std_info(
                    DEFAULT_TIMES,
                    ARCHIVE | win32::FILE_ATTRIBUTE_REPARSE_POINT | extra,
                )
                .name(ROOT, &format!("cloud{n}.docx"), NS_WIN32_AND_DOS)
                .data_sparse("", NonResidentSpec::new(sparse_run(100), 400_000, 4096), 0)
                .reparse(raw_reparse(ONEDRIVE, &[0u8; 24])),
        );
    }
    let (recs, _) = scan(b.finish());
    for (n, extra, state) in cases {
        let attrs = ARCHIVE | win32::FILE_ATTRIBUTE_REPARSE_POINT | extra;
        let mut want = base_record(
            fref(n),
            vec![link(ROOT, &format!("cloud{n}.docx"))],
            attrs,
            sizes(400_000, 0),
        );
        want.flags = want
            .flags
            .with_reparse(ReparseKind::Cloud)
            .with_cloud(state);
        want.reparse = Some(Reparse {
            tag: ONEDRIVE,
            target: None,
        });
        assert_eq!(recs[&n], want, "record {n}");
    }
}

#[test]
fn symlink_junction_and_unknown_reparse_tags() {
    let mut b = img();
    b.insert(
        120,
        RecordBuilder::file(1, ROOT, "link.txt")
            .data("", &[])
            .reparse(symlink_reparse(
                r"\??\C:\real\file.txt",
                r"C:\real\file.txt",
                false,
            )),
    );
    b.insert(
        121,
        RecordBuilder::dir(1, ROOT, "junction").reparse(mount_point_reparse(r"\??\D:\target", "")),
    );
    b.insert(
        122,
        RecordBuilder::file(1, ROOT, "relative")
            .data("", &[])
            .reparse(symlink_reparse(r"..\x", r"..\x", true)),
    );
    b.insert(
        123,
        RecordBuilder::file(1, ROOT, "odd")
            .data("", &[])
            .reparse(raw_reparse(0x1234_5678, &[9; 4])),
    );
    b.insert(
        124,
        RecordBuilder::file(1, ROOT, "wsl")
            .data("", &[])
            .reparse(raw_reparse(
                win32::IO_REPARSE_TAG_LX_SYMLINK,
                &[2, 0, 0, 0, b'/'],
            )),
    );
    let (recs, _) = scan(b.finish());

    let mut s = base_record(
        fref(120),
        vec![link(ROOT, "link.txt")],
        ARCHIVE,
        sizes(0, 0),
    );
    s.flags = s.flags.with_reparse(ReparseKind::Symlink);
    s.reparse = Some(Reparse {
        tag: win32::IO_REPARSE_TAG_SYMLINK,
        target: Some(WideName::from_str_lossless(r"C:\real\file.txt")),
    });
    assert_eq!(recs[&120], s);

    // Empty print name falls back to the substitute name.
    let mut j = base_record(
        fref(121),
        vec![link(ROOT, "junction")],
        win32::FILE_ATTRIBUTE_DIRECTORY,
        sizes(0, 0),
    );
    j.flags = EntryFlags::DIR.with_reparse(ReparseKind::MountPoint);
    j.reparse = Some(Reparse {
        tag: win32::IO_REPARSE_TAG_MOUNT_POINT,
        target: Some(WideName::from_str_lossless(r"\??\D:\target")),
    });
    assert_eq!(recs[&121], j);
    assert!(recs[&121].flags.reparse().blocks_traversal());

    assert_eq!(
        recs[&122].reparse.as_ref().unwrap().target,
        Some(WideName::from_str_lossless(r"..\x"))
    );
    assert_eq!(recs[&123].flags.reparse(), ReparseKind::Unknown);
    assert_eq!(
        recs[&123].reparse,
        Some(Reparse {
            tag: 0x1234_5678,
            target: None
        })
    );
    assert_eq!(recs[&124].flags.reparse(), ReparseKind::Wsl);
}

#[test]
fn many_ads_and_a_huge_sparse_ads() {
    let g = Geometry {
        record_size: 4096,
        ..Geometry::default()
    };
    let mut b = ImageBuilder::new(g)
        .with_system_files()
        .claim_clusters(1 << 30);
    let one = b.alloc(1);
    let huge_runs = chain_runs(0, &[one, sparse_run((1 << 31) - 1)]);
    let mut r = RecordBuilder::file(1, ROOT, "streams.dat").data("", b"main");
    let mut want_ads = Vec::new();
    for i in 0..40 {
        let name = format!("s{i:02}");
        r = r.data(&name, &vec![b'x'; i]);
        want_ads.push(AdsInfo {
            name: WideName::from_str_lossless(&name),
            logical: i as u64,
            allocated: 0,
        });
    }
    let huge_len = 8u64 << 40;
    r = r.data_sparse("huge", NonResidentSpec::new(huge_runs, huge_len, 4096), CS);
    want_ads.push(AdsInfo {
        name: WideName::from_str_lossless("huge"),
        logical: huge_len,
        allocated: CS,
    });
    b.insert(130, r);
    let (recs, _) = scan(b.finish());
    let mut want = base_record(
        fref(130),
        vec![link(ROOT, "streams.dat")],
        ARCHIVE,
        Sizes {
            logical: 4,
            allocated: 0,
            ads_logical: (0..40).sum::<u64>() + huge_len,
            ads_allocated: CS,
            dir_overhead: 0,
            attr_overhead: 0,
        },
    );
    want.flags |= EntryFlags::HAS_ADS;
    want.ads = want_ads;
    assert_eq!(recs[&130], want);
}

#[test]
fn directory_index_allocation_is_dir_overhead() {
    let mut b = img();
    let runs = b.alloc(4);
    b.insert(
        140,
        RecordBuilder::dir(1, ROOT, "big dir")
            .index_allocation("$I30", NonResidentSpec::new(runs, 4 * CS, 4096)),
    );
    let (recs, _) = scan(b.finish());
    let mut want = base_record(
        fref(140),
        vec![link(ROOT, "big dir")],
        win32::FILE_ATTRIBUTE_DIRECTORY,
        Sizes {
            dir_overhead: 4 * CS,
            ..Sizes::default()
        },
    );
    want.flags = EntryFlags::DIR;
    assert_eq!(recs[&140], want);
}

/// Base 150 holds an attribute list; extension 140 (before the base) holds
/// `$DATA` VCN 0 and a hardlink; extension 160 (after) holds the `$DATA`
/// continuation and an ADS.
fn split_record_image(list_nonresident: bool) -> (Vec<u8>, ScanRecord) {
    let mut b = img();
    let head = b.alloc(4);
    let tail = chain_runs(4, &[b.alloc(6)]);
    let base = FileRef::from_parts(150, 3);
    let list = attr_list_value(&[
        AttrListSpec::new(AT_STANDARD_INFORMATION, 0, base),
        AttrListSpec::new(AT_FILE_NAME, 0, base),
        AttrListSpec::new(AT_FILE_NAME, 0, FileRef::from_parts(140, 1)),
        AttrListSpec::new(AT_DATA, 0, FileRef::from_parts(140, 1)),
        AttrListSpec::new(AT_DATA, 4, FileRef::from_parts(160, 1)),
        AttrListSpec {
            name: "meta".encode_utf16().collect(),
            ..AttrListSpec::new(AT_DATA, 0, FileRef::from_parts(160, 1))
        },
    ]);
    let mut rec = RecordBuilder::new(3).std_info(DEFAULT_TIMES, ARCHIVE);
    rec = if list_nonresident {
        let runs = b.alloc(1);
        b.write_runs(&runs, &list);
        rec.attr(
            AT_ATTRIBUTE_LIST,
            "",
            0,
            AttrValue::NonResident(NonResidentSpec::new(runs, list.len() as u64, 4096)),
        )
    } else {
        rec.attr(AT_ATTRIBUTE_LIST, "", 0, AttrValue::Resident(list))
    };
    rec = rec.name(ROOT, "split.bin", NS_WIN32_AND_DOS);
    b.insert(150, rec);
    let mut data0 = NonResidentSpec::new(head, 40_000, 4096);
    data0.allocated = 10 * CS;
    b.insert(
        140,
        RecordBuilder::new(1)
            .extension_of(base)
            .name(fref(70), "alias.bin", NS_WIN32)
            .data_nonresident("", 0, data0),
    );
    b.insert(
        160,
        RecordBuilder::new(1)
            .extension_of(base)
            .data_nonresident("", 0, NonResidentSpec::continuation(tail))
            .data("meta", b"0123456789"),
    );
    let mut want = base_record(
        base,
        vec![link(ROOT, "split.bin"), link(fref(70), "alias.bin")],
        ARCHIVE,
        Sizes {
            logical: 40_000,
            allocated: 10 * CS,
            ads_logical: 10,
            ads_allocated: 0,
            dir_overhead: 0,
            // A non-resident attribute list occupies its one cluster on disk.
            attr_overhead: if list_nonresident { CS } else { 0 },
        },
    );
    want.flags |= EntryFlags::HAS_ADS;
    want.ads = vec![AdsInfo {
        name: WideName::from_str_lossless("meta"),
        logical: 10,
        allocated: 0,
    }];
    (b.finish(), want)
}

#[test]
fn extension_records_before_and_after_base_are_merged() {
    for nonresident in [false, true] {
        let (image, want) = split_record_image(nonresident);
        let (recs, stats) = scan(image.clone());
        assert_eq!(recs[&150], want, "nonresident list: {nonresident}");
        assert!(!recs.contains_key(&140) && !recs.contains_key(&160));
        assert_eq!(stats.extension_records, 2);
        assert_eq!(stats.extensions_merged, 2);
        assert_eq!(stats.extensions_orphaned, 0);
        // Single-record reads follow the attribute list to the same result.
        let vol = NtfsVolume::open(image).unwrap();
        assert_eq!(vol.read_record(150).unwrap(), Some(want));
        assert_eq!(vol.read_record(140).unwrap(), None);
    }
}

#[test]
fn stale_extension_records_are_not_merged() {
    let mut b = img();
    b.insert(
        170,
        RecordBuilder::new(2)
            .std_info(DEFAULT_TIMES, ARCHIVE)
            .attr_list(&[AttrListSpec::new(AT_DATA, 0, fref(171))])
            .name(ROOT, "base", NS_WIN32_AND_DOS),
    );
    // Points at sequence 1 of record 170: left over from a previous file.
    b.insert(
        171,
        RecordBuilder::new(1)
            .extension_of(FileRef::from_parts(170, 1))
            .data("", b"stale"),
    );
    // Base without an attribute list.
    b.insert(
        172,
        RecordBuilder::new(1).extension_of(fref(66)).data("", b"x"),
    );
    let (recs, stats) = scan(b.finish());
    assert_eq!(recs[&170].sizes, sizes(0, 0));
    assert_eq!(stats.extensions_orphaned, 2);
    assert_eq!(stats.extensions_merged, 0);
}

#[test]
fn corrupt_torn_garbage_and_malformed_records_are_counted_and_skipped() {
    let mut b = img().min_records(64);
    b.insert(40, RecordBuilder::file(1, ROOT, "baad").baad());
    b.insert(
        41,
        RecordBuilder::file(1, ROOT, "torn").data("", b"t").torn(),
    );
    b.insert(
        42,
        RecordBuilder::file(1, ROOT, "garbage").signature(*b"JUNK"),
    );
    let mut bad = RecordBuilder::file(1, ROOT, "bad attr").build(Geometry::default(), 43);
    // Attribute length past the used size. The field is not under a stride
    // tail, so the fixups stay valid and only the attribute walk fails.
    bad[0x38 + 4..0x38 + 8].copy_from_slice(&0x0FF0u32.to_le_bytes());
    b.insert_raw(43, bad);
    b.insert(44, RecordBuilder::file(1, ROOT, "fine").data("", b"ok"));
    let (recs, stats) = scan(b.finish());
    assert_eq!(
        (
            stats.corrupt,
            stats.torn,
            stats.bad_signature,
            stats.malformed
        ),
        (1, 1, 1, 1)
    );
    assert!(recs.contains_key(&44));
    for n in 40..44 {
        assert!(!recs.contains_key(&n));
    }
    let vol_err = |img: Vec<u8>, n| NtfsVolume::open(img).unwrap().read_record(n).is_err();
    let mut b = img();
    b.insert(40, RecordBuilder::file(1, ROOT, "baad").baad());
    let image = b.finish();
    assert!(vol_err(image.clone(), 40));
    assert_eq!(
        NtfsVolume::open(image).unwrap().read_record(41).unwrap(),
        None
    );
}

#[test]
fn orphans_and_parent_cycles_are_emitted_faithfully() {
    let mut b = img();
    // Parent record 999 does not exist; parent 64 exists but with sequence 2.
    b.insert(64, RecordBuilder::dir(2, ROOT, "reused"));
    b.insert(
        180,
        RecordBuilder::file(1, fref(999), "lost.txt").data("", b"1"),
    );
    b.insert(
        181,
        RecordBuilder::file(1, fref(64), "stale parent.txt").data("", b"2"),
    );
    // 182 <-> 183 form a cycle.
    b.insert(182, RecordBuilder::dir(1, fref(183), "loop a"));
    b.insert(183, RecordBuilder::dir(1, fref(182), "loop b"));
    let (recs, _) = scan(b.finish());
    assert_eq!(recs[&180].links, vec![link(fref(999), "lost.txt")]);
    assert_eq!(recs[&181].links, vec![link(fref(64), "stale parent.txt")]);
    assert_eq!(recs[&64].id, FileRef::from_parts(64, 2));
    assert_eq!(recs[&182].links, vec![link(fref(183), "loop a")]);
    assert_eq!(recs[&183].links, vec![link(fref(182), "loop b")]);
    assert!(!recs[&182].flags.contains(EntryFlags::ORPHAN));
}

#[test]
fn system_files_root_and_extend_children_are_flagged() {
    let mut b = img();
    b.insert(
        24,
        RecordBuilder::new(24)
            .std_info(
                DEFAULT_TIMES,
                win32::FILE_ATTRIBUTE_HIDDEN | win32::FILE_ATTRIBUTE_SYSTEM,
            )
            .name(EXTEND, "$UsnJrnl", NS_WIN32_AND_DOS),
    );
    b.insert(25, RecordBuilder::file(1, ROOT, "user.txt").data("", &[]));
    let image = b.finish();
    let vol = NtfsVolume::open(image.clone()).unwrap();
    let cs = vol.boot().cluster_size;
    let total = vol.boot().total_clusters;
    let (recs, _) = scan(image);
    for n in [0u64, 1, 2, 3, 4, 6, 7, 8, 9, 10, 11, 24] {
        assert!(
            recs[&n].flags.contains(EntryFlags::NTFS_METADATA),
            "record {n}"
        );
    }
    let root = &recs[&5];
    assert!(!root.flags.contains(EntryFlags::NTFS_METADATA));
    assert!(root.is_dir());
    assert_eq!(root.links, vec![link(ROOT, ".")]);
    assert_eq!(root.id, ROOT);
    assert!(!recs[&25].flags.contains(EntryFlags::NTFS_METADATA));
    // $BadClus:$Bad claims the whole volume but has no real clusters.
    let bad = &recs[&8];
    assert_eq!(
        bad.ads,
        vec![AdsInfo {
            name: WideName::from_str_lossless("$Bad"),
            logical: total * cs,
            allocated: 0
        }]
    );
    assert_eq!(bad.sizes.total_allocated(), 0);
    // Metadata streams stay out of user-facing ADS logical totals.
    assert_eq!(bad.sizes.ads_logical, 0);
    assert!(
        recs.values()
            .all(|r| !r.flags.contains(EntryFlags::NTFS_METADATA) || r.sizes.ads_logical == 0)
    );
    // $MFT reports its own data as allocated.
    assert_eq!(
        recs[&0].sizes.allocated,
        vol.layout().runs.iter().map(|r| r.len).sum::<u64>() * cs
    );
}

#[test]
fn records_without_names_or_std_info_are_still_emitted() {
    let mut b = img();
    b.insert(190, RecordBuilder::new(4).data("", b"abc"));
    let (recs, _) = scan(b.finish());
    let r = &recs[&190];
    assert_eq!(
        *r,
        ScanRecord {
            id: FileRef::from_parts(190, 4),
            links: vec![],
            attributes: 0,
            flags: EntryFlags::EMPTY,
            times: Times::default(),
            fn_created: None,
            sizes: sizes(3, 0),
            reparse: None,
            ads: vec![],
        }
    );
}
