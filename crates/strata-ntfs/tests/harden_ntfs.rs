//! Synthetic-image edge cases beyond the per-case suite: 1024 hardlinks and
//! ~300 streams spread over extension records through a non-resident
//! attribute list, every power-of-two cluster size at 512-byte and 4Kn
//! sectors, a 4Kn `$MFT` in ~400 fragments, torn last strides and torn
//! extension records, and a 100 GB sparse file with 1 MB allocated.

mod common;

use common::{link, scan, sizes};
use strata_core::{AdsInfo, EntryFlags, FileRef, NameLink, ScanRecord, Sizes, WideName, win32};
use strata_ntfs::test_image::{
    AttrListSpec, AttrValue, DEFAULT_TIMES, Geometry, ImageBuilder, NonResidentSpec, ROOT,
    RecordBuilder, attr_list_value, chain_runs, sparse_run,
};
use strata_ntfs::{
    AT_ATTRIBUTE_LIST, AT_DATA, AT_FILE_NAME, AT_STANDARD_INFORMATION, FIXUP_STRIDE, NS_WIN32,
    NtfsVolume,
};

const ARCHIVE: u32 = win32::FILE_ATTRIBUTE_ARCHIVE;

/// One attribute to place in an extension record.
enum Item {
    Name(FileRef, String),
    Data(String, Vec<u8>),
    DataNr(String, NonResidentSpec),
}

/// Builds a base record at `base_no` whose attributes live in extension
/// records `ext_start..`, `per_record` items each, listed by a non-resident
/// attribute list. Returns the attribute list's cluster count.
fn spread(
    b: &mut ImageBuilder,
    base_no: u64,
    ext_start: u64,
    items: Vec<Item>,
    per_record: usize,
) -> u64 {
    let base = FileRef::from_parts(base_no, 1);
    let mut list = vec![AttrListSpec::new(AT_STANDARD_INFORMATION, 0, base)];
    let mut ext_no = ext_start;
    let mut items = items.into_iter().peekable();
    while items.peek().is_some() {
        let holder = FileRef::from_parts(ext_no, 1);
        let mut rec = RecordBuilder::new(1).extension_of(base);
        for (id, item) in items.by_ref().take(per_record).enumerate() {
            let id = id as u16;
            match item {
                Item::Name(parent, name) => {
                    rec = rec.name(parent, &name, NS_WIN32);
                    list.push(AttrListSpec {
                        id,
                        ..AttrListSpec::new(AT_FILE_NAME, 0, holder)
                    });
                }
                Item::Data(name, bytes) => {
                    rec = rec.data(&name, &bytes);
                    list.push(AttrListSpec {
                        id,
                        name: name.encode_utf16().collect(),
                        ..AttrListSpec::new(AT_DATA, 0, holder)
                    });
                }
                Item::DataNr(name, spec) => {
                    let vcn = spec.start_vcn;
                    rec = rec.data_nonresident(&name, 0, spec);
                    list.push(AttrListSpec {
                        id,
                        name: name.encode_utf16().collect(),
                        ..AttrListSpec::new(AT_DATA, vcn, holder)
                    });
                }
            }
        }
        b.insert(ext_no, rec);
        ext_no += 1;
    }
    let value = attr_list_value(&list);
    let cs = u64::from(b.cluster_size());
    let clusters = (value.len() as u64).div_ceil(cs);
    let runs = b.alloc(clusters);
    b.write_runs(&runs, &value);
    b.insert(
        base_no,
        RecordBuilder::new(1).std_info(DEFAULT_TIMES, ARCHIVE).attr(
            AT_ATTRIBUTE_LIST,
            "",
            0,
            AttrValue::NonResident(NonResidentSpec::new(
                runs,
                value.len() as u64,
                b.cluster_size(),
            )),
        ),
    );
    clusters
}

fn assert_read_record_matches_scan(image: Vec<u8>, n: u64) -> ScanRecord {
    let (recs, stats) = scan(image.clone());
    assert_eq!(stats.extensions_orphaned, 0);
    let vol = NtfsVolume::open(image).unwrap();
    let scanned = recs[&n].clone();
    assert!(!scanned.flags.contains(EntryFlags::PARTIAL));
    assert_eq!(vol.read_record(n).unwrap(), Some(scanned.clone()));
    scanned
}

#[test]
fn a_thousand_hardlinks_across_extension_records() {
    let mut b = ImageBuilder::new(Geometry::default()).with_system_files();
    let dirs: Vec<FileRef> = (0..10).map(|i| FileRef::from_parts(64 + i, 1)).collect();
    for (i, d) in dirs.iter().enumerate() {
        b.insert(d.record(), RecordBuilder::dir(1, ROOT, &format!("d{i}")));
    }
    let names: Vec<(FileRef, String)> = (0..1024)
        .map(|i| (dirs[i % dirs.len()], format!("link-{i:04}")))
        .collect();
    let items = names
        .iter()
        .map(|(p, n)| Item::Name(*p, n.clone()))
        .collect();
    let list_clusters = spread(&mut b, 200, 201, items, 8);
    assert!(
        list_clusters > 1,
        "the attribute list spans several clusters"
    );
    let rec = assert_read_record_matches_scan(b.finish(), 200);
    let want: Vec<NameLink> = names.iter().map(|(p, n)| link(*p, n)).collect();
    assert_eq!(rec.links.len(), 1024);
    assert_eq!(rec.links, want);
    assert_eq!(rec.sizes.attr_overhead, list_clusters * 4096);
}

#[test]
fn three_hundred_streams_across_extension_records_with_a_continuation() {
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .claim_clusters(1 << 20);
    let head = b.alloc(3);
    let tail = chain_runs(3, &[b.alloc(5)]);
    let mut big = NonResidentSpec::new(head, 30_000, 4096);
    big.allocated = 8 * 4096;
    let mut items = vec![Item::Name(ROOT, "streams.bin".into())];
    items.push(Item::Data(String::new(), b"main".to_vec()));
    let mut want_ads = Vec::new();
    for i in 0..300usize {
        let name = format!("s{i:03}");
        let bytes = vec![b'x'; i % 17];
        want_ads.push(AdsInfo {
            name: WideName::from_str_lossless(&name),
            logical: bytes.len() as u64,
            allocated: 0,
        });
        items.push(Item::Data(name, bytes));
        if i == 150 {
            // The first piece of a non-resident stream lands mid-way; its
            // continuation is placed last, many records later.
            items.push(Item::DataNr("big".into(), big.clone()));
            want_ads.push(AdsInfo {
                name: WideName::from_str_lossless("big"),
                logical: 30_000,
                allocated: 8 * 4096,
            });
        }
    }
    items.push(Item::DataNr(
        "big".into(),
        NonResidentSpec::continuation(tail),
    ));
    let list_clusters = spread(&mut b, 300, 301, items, 20);
    let rec = assert_read_record_matches_scan(b.finish(), 300);
    assert_eq!(rec.links, vec![link(ROOT, "streams.bin")]);
    assert_eq!(rec.ads.len(), 301);
    let mut got = rec.ads.clone();
    got.sort_by(|a, b| a.name.units().cmp(b.name.units()));
    want_ads.sort_by(|a, b| a.name.units().cmp(b.name.units()));
    assert_eq!(got, want_ads);
    assert_eq!(
        rec.sizes,
        Sizes {
            logical: 4,
            allocated: 0,
            ads_logical: want_ads.iter().map(|a| a.logical).sum(),
            ads_allocated: 8 * 4096,
            dir_overhead: 0,
            attr_overhead: list_clusters * 4096,
        }
    );
    assert!(rec.flags.contains(EntryFlags::HAS_ADS));
}

#[test]
fn every_cluster_size_at_512_and_4k_sectors() {
    for sector in [512u32, 4096] {
        let mut cluster = sector;
        while cluster <= 2 * 1024 * 1024 {
            let g = Geometry {
                sector_size: sector,
                cluster_size: cluster,
                record_size: if sector == 4096 { 4096 } else { 1024 },
            };
            let cs = u64::from(cluster);
            let mut b = ImageBuilder::new(g)
                .with_system_files()
                .mft_fragments(4)
                .mft_data_in_extension(15);
            let data = b.alloc(3);
            let real = b.alloc(1);
            let sparse = chain_runs(0, &[real, sparse_run(7)]);
            b.insert(64, RecordBuilder::file(1, ROOT, "r.txt").data("", b"abc"));
            b.insert(
                65,
                RecordBuilder::file(1, ROOT, "n.bin").data_nonresident(
                    "",
                    0,
                    NonResidentSpec::new(data, 2 * cs + 7, cluster),
                ),
            );
            b.insert(
                66,
                RecordBuilder::file(1, ROOT, "s.bin").data_sparse(
                    "",
                    NonResidentSpec::new(sparse, 8 * cs, cluster),
                    cs,
                ),
            );
            let image = b.finish();
            let vol = NtfsVolume::open(image.clone()).unwrap();
            assert_eq!(vol.boot().cluster_size, cs, "{g:?}");
            let (recs, stats) = scan(image);
            assert_eq!(
                stats.corrupt + stats.torn + stats.malformed + stats.bad_signature,
                0,
                "{g:?}"
            );
            assert_eq!(recs[&64].sizes, sizes(3, 0), "{g:?}");
            assert_eq!(recs[&65].sizes, sizes(2 * cs + 7, 3 * cs), "{g:?}");
            assert_eq!(recs[&66].sizes.logical, 8 * cs, "{g:?}");
            assert_eq!(recs[&66].sizes.allocated, cs, "{g:?}");
            for n in [0u64, 5, 64, 65, 66] {
                assert_eq!(
                    vol.read_record(n).unwrap().as_ref(),
                    recs.get(&n),
                    "{g:?} {n}"
                );
            }
            let used = vol.count_used_clusters().unwrap() * cs;
            let found: u64 = recs.values().map(|r| r.sizes.total_allocated()).sum();
            assert_eq!(used, found, "{g:?}");
            cluster *= 2;
        }
    }
}

#[test]
fn four_k_native_mft_in_four_hundred_fragments() {
    let g = Geometry {
        sector_size: 4096,
        cluster_size: 4096,
        record_size: 4096,
    };
    let mut b = ImageBuilder::new(g)
        .with_system_files()
        .mft_fragments(400)
        .min_records(1200)
        .mft_data_in_extension(15);
    for n in [16u64, 399, 400, 401, 799, 1199] {
        b.insert(
            n,
            RecordBuilder::file(1, ROOT, &format!("f{n}")).data("", b"y"),
        );
    }
    let image = b.finish();
    let vol = NtfsVolume::open(image.clone()).unwrap();
    assert_eq!(vol.layout().fragment_count(), 400);
    let (recs, stats) = scan(image);
    assert!(!recs.contains_key(&15), "the $MFT extension is merged");
    assert_eq!(stats.extensions_merged, 1);
    for n in [16u64, 399, 400, 401, 799, 1199] {
        assert_eq!(recs[&n].links, vec![link(ROOT, &format!("f{n}"))]);
        assert_eq!(vol.read_record(n).unwrap().as_ref(), recs.get(&n));
    }
}

#[test]
fn torn_last_stride_is_detected() {
    for g in [
        Geometry::default(),
        Geometry {
            sector_size: 4096,
            cluster_size: 4096,
            record_size: 4096,
        },
    ] {
        let mut b = ImageBuilder::new(g).with_system_files().min_records(64);
        let mut raw = RecordBuilder::file(1, ROOT, "torn-tail")
            .data("", b"t")
            .build(g, 40);
        let last = raw.len() - 1;
        assert_eq!(raw.len() % FIXUP_STRIDE, 0);
        raw[last] ^= 0xFF;
        b.insert_raw(40, raw);
        b.insert(41, RecordBuilder::file(1, ROOT, "fine").data("", b"ok"));
        let image = b.finish();
        let (recs, stats) = scan(image.clone());
        assert_eq!(stats.torn, 1, "{g:?}");
        assert!(!recs.contains_key(&40));
        assert!(recs.contains_key(&41));
        // A single-record read reports the damage instead of guessing.
        assert!(NtfsVolume::open(image).unwrap().read_record(40).is_err());
    }
}

#[test]
fn torn_extension_record_never_corrupts_its_base() {
    let mut b = ImageBuilder::new(Geometry::default()).with_system_files();
    let items: Vec<Item> = (0..24)
        .map(|i| Item::Name(ROOT, format!("name-{i:02}")))
        .collect();
    spread(&mut b, 200, 201, items, 8);
    // Tear the middle extension record (names 8..16).
    let raw = RecordBuilder::new(1)
        .extension_of(FileRef::from_parts(200, 1))
        .name(ROOT, "torn-a", NS_WIN32)
        .torn()
        .build(Geometry::default(), 202);
    b.insert_raw(202, raw);
    let image = b.finish();
    let (recs, stats) = scan(image.clone());
    assert_eq!(stats.torn, 1);
    let rec = &recs[&200];
    // The base keeps every name it can prove and none from the torn record.
    let names: Vec<String> = rec.links.iter().map(|l| l.name.to_string_lossy()).collect();
    assert!(!names.iter().any(|n| n.starts_with("torn")), "{names:?}");
    for i in (0..8).chain(16..24) {
        assert!(names.contains(&format!("name-{i:02}")), "{i}: {names:?}");
    }
    // Attributes the list names but nobody could read: the record says so.
    assert!(rec.flags.contains(EntryFlags::PARTIAL));
    let vol = NtfsVolume::open(image).unwrap();
    assert_eq!(vol.read_record(200).unwrap().as_ref(), Some(rec));
}

#[test]
fn hundred_gigabyte_sparse_file_with_one_megabyte_allocated() {
    const GB100: u64 = 100 * 1000 * 1000 * 1000;
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .claim_clusters(GB100 / 4096 + 1024);
    let real = b.alloc(256);
    let runs = chain_runs(0, &[real, sparse_run(GB100.div_ceil(4096) - 256)]);
    b.insert(
        64,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, ARCHIVE | win32::FILE_ATTRIBUTE_SPARSE_FILE)
            .name(ROOT, "huge.vhdx", NS_WIN32)
            .data_sparse("", NonResidentSpec::new(runs, GB100, 4096), 1 << 20),
    );
    let image = b.finish();
    assert!(image.len() < 64 << 20, "the image stays small");
    let (recs, _) = scan(image.clone());
    let r = &recs[&64];
    assert_eq!(r.sizes.logical, GB100);
    assert_eq!(r.sizes.allocated, 1 << 20);
    assert!(r.flags.contains(EntryFlags::SPARSE));
    let vol = NtfsVolume::open(image).unwrap();
    assert_eq!(vol.read_record(64).unwrap().as_ref(), Some(r));
}
