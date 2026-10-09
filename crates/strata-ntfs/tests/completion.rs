//! Completion of attribute-list records during a full scan: the scan reuses
//! the extension records it already read and reads non-resident lists in
//! queued batches, and must emit exactly what single-record reads produce.

use std::collections::BTreeMap;

use strata_core::{EntryFlags, FileRef, ScanRecord};
use strata_ntfs::test_image::{
    AttrListSpec, AttrValue, Geometry, ImageBuilder, NonResidentSpec, ROOT, RecordBuilder,
    attr_list_value,
};
use strata_ntfs::{
    AT_ATTRIBUTE_LIST, AT_DATA, AT_FILE_NAME, AT_STANDARD_INFORMATION, IoMode, NS_WIN32,
    NtfsVolume, RawVolume, ReadAt, Run, ScanOptions,
};

fn fref(n: u64) -> FileRef {
    FileRef::from_parts(n, 1)
}

/// Groups of a base plus one to three extension records holding names and
/// data, with resident and non-resident lists (some fragmented), some
/// extensions before their base, some holders free (partial records) and
/// one list whose runs lie past the end of the image (unreadable).
fn image(g: Geometry) -> (Vec<u8>, Vec<u64>, u64) {
    let cs = u64::from(g.cluster_size);
    let mut b = ImageBuilder::new(g).with_system_files().min_records(2000);
    let mut bases = Vec::new();
    for (i, start) in (40..1900u64).step_by(6).enumerate() {
        let count = 1 + (i % 3) as u64;
        let (base, first) = if i % 4 == 0 {
            (start + count, start)
        } else {
            (start, start + 1)
        };
        let holders: Vec<u64> = (first..first + count).collect();
        let me = fref(base);
        let mut entries = vec![
            AttrListSpec::new(AT_STANDARD_INFORMATION, 0, me),
            AttrListSpec::new(AT_FILE_NAME, 0, me),
        ];
        for &h in &holders {
            entries.push(AttrListSpec::new(AT_FILE_NAME, 0, fref(h)));
        }
        entries.push(AttrListSpec::new(AT_DATA, 0, fref(holders[0])));
        let list = attr_list_value(&entries);
        let f = RecordBuilder::file(1, ROOT, &format!("base{base}"));
        let rec = if i % 2 == 0 {
            f.attr(AT_ATTRIBUTE_LIST, "", 0, AttrValue::Resident(list))
        } else {
            // Fragmented over clusters that are rarely sector-group aligned.
            let clusters = (list.len() as u64).div_ceil(cs);
            let mut runs = Vec::new();
            for c in 0..clusters {
                let r = b.alloc(1)[0];
                b.alloc(1);
                runs.push(Run { vcn: c, ..r });
            }
            b.write_runs(&runs, &list);
            f.attr(
                AT_ATTRIBUTE_LIST,
                "",
                0,
                AttrValue::NonResident(NonResidentSpec::new(
                    runs,
                    list.len() as u64,
                    g.cluster_size,
                )),
            )
        };
        b.insert(base, rec);
        for (k, &h) in holders.iter().enumerate() {
            // Every seventh group loses its last holder: the base is partial.
            if i % 7 == 3 && k + 1 == holders.len() && k > 0 {
                continue;
            }
            let mut e =
                RecordBuilder::new(1)
                    .extension_of(me)
                    .name(ROOT, &format!("link{h}"), NS_WIN32);
            if k == 0 {
                e = e.data("", &vec![1u8; (h % 200) as usize]);
            }
            b.insert(h, e);
        }
        bases.push(base);
    }
    // A non-resident list whose runs lie past the end of the image.
    let lost = 1950;
    let list = attr_list_value(&[AttrListSpec::new(AT_DATA, 0, fref(1951))]);
    b.insert(
        lost,
        RecordBuilder::file(1, ROOT, "lost").attr(
            AT_ATTRIBUTE_LIST,
            "",
            0,
            AttrValue::NonResident(NonResidentSpec::new(
                vec![Run {
                    vcn: 0,
                    lcn: Some(9_000_000),
                    len: 1,
                }],
                list.len() as u64,
                g.cluster_size,
            )),
        ),
    );
    b.insert(
        1951,
        RecordBuilder::new(1)
            .extension_of(fref(lost))
            .data("", b"x"),
    );
    (b.claim_clusters(10_000_000).finish(), bases, lost)
}

fn scan<R: ReadAt + Sync>(vol: &NtfsVolume<R>, opts: &ScanOptions) -> Vec<ScanRecord> {
    let mut out = Vec::new();
    let stats = vol.scan(opts, |b| out.extend(b)).unwrap();
    assert_eq!(stats.records_emitted as usize, out.len());
    assert_eq!(stats.extensions_orphaned, 0);
    out
}

#[test]
fn scan_completion_matches_single_record_reads_in_every_io_mode() {
    let tmp = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    for (gi, g) in [
        Geometry::default(),
        Geometry {
            sector_size: 512,
            cluster_size: 512,
            record_size: 1024,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let (img, bases, lost) = image(g);
        let mem = NtfsVolume::open(img.clone()).unwrap();
        let want = scan(&mem, &ScanOptions::default());
        let by_record: BTreeMap<u64, &ScanRecord> =
            want.iter().map(|r| (r.id.record(), r)).collect();
        let mut partial = 0;
        for &n in &bases {
            let single = mem.read_record(n).unwrap().unwrap();
            assert_eq!(by_record[&n], &single, "record {n}");
            partial += usize::from(single.flags.contains(EntryFlags::PARTIAL));
        }
        assert!(partial > 10, "{partial}");
        assert!(by_record[&lost].flags.contains(EntryFlags::PARTIAL));
        assert!(mem.read_record(lost).is_err());

        let path = tmp.join(format!("strata-ntfs-completion-{gi}.img"));
        std::fs::write(&path, &img).unwrap();
        for mode in [IoMode::NoBuffering, IoMode::Sequential] {
            let vol = NtfsVolume::open(RawVolume::open(&path, mode).unwrap()).unwrap();
            for io_depth in [1, 8] {
                for use_mft_bitmap in [false, true] {
                    let opts = ScanOptions {
                        io_depth,
                        use_mft_bitmap,
                        chunk_bytes: 64 * 1024,
                        ..ScanOptions::default()
                    };
                    let got = scan(&vol, &opts);
                    assert!(
                        got == want,
                        "{g:?} {mode:?} depth {io_depth} bitmap {use_mft_bitmap}"
                    );
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}
