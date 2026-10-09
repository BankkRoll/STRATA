//! End-to-end pipeline tests on synthetic byte-level event streams:
//! decode → process/file mapping → aggregation → store-shaped output.

mod common;

use common::*;
use strata_etw::aggregate::{Counts, Window};
use strata_etw::decode::{Decoder, EventKind};
use strata_etw::evidence::EvidenceConfig;
use strata_store::{Timestamp, path_hash};

const APP: &str = r"\Device\HarddiskVolume3\Program Files\Tool\tool.exe";
const APP_DOS: &str = r"C:\Program Files\Tool\tool.exe";

// -----------------------------------------------------------------------------
// Decoding of synthetic fixtures
// -----------------------------------------------------------------------------

#[test]
fn decodes_every_fixture_kind() {
    let d = Decoder::default();
    let got = |e: &Ev| d.decode(&e.raw()).unwrap().unwrap().kind;
    assert_eq!(
        got(&write(10, T0, 0xA0, 0xB0, 512)),
        EventKind::Write {
            file_object: 0xA0,
            key: 0xB0,
            size: 512,
            io_flags: 0
        }
    );
    // 32-bit pointers (WOW64 header flag) shift every later field.
    assert_eq!(
        got(&write_ptr(10, T0, 0xA0, 0xB0, 99, 0x43, 4)),
        EventKind::Write {
            file_object: 0xA0,
            key: 0xB0,
            size: 99,
            io_flags: 0x43
        }
    );
    match got(&create(
        10,
        T0,
        0xA0,
        0x1000,
        r"\Device\HarddiskVolume3\x\y.txt",
    )) {
        EventKind::Create {
            file_object,
            options,
            name,
        } => {
            assert_eq!((file_object, options), (0xA0, 0x1000));
            assert_eq!(
                String::from_utf16(&name).unwrap(),
                r"\Device\HarddiskVolume3\x\y.txt"
            );
        }
        k => panic!("{k:?}"),
    }
    match got(&proc_start(321, T0, APP)) {
        EventKind::ProcessStart {
            pid,
            create_time,
            image,
        } => {
            assert_eq!((pid, create_time), (321, ft(T0)));
            assert_eq!(String::from_utf16(&image).unwrap(), APP);
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(
        got(&proc_stop(321, T0, T0 + 5)),
        EventKind::ProcessStop {
            pid: 321,
            create_time: ft(T0)
        }
    );
    assert!(matches!(
        got(&rename_path(1, T0, 2, 3, r"\x")),
        EventKind::RenamePath { .. }
    ));
    assert!(matches!(
        got(&name_delete(T0, 3, r"\x")),
        EventKind::NameDelete { key: 3, .. }
    ));
    // Undelete (disposition cleared) decodes to nothing.
    assert_eq!(
        d.decode(&delete_path(1, T0, 2, 3, false, r"\x").raw()),
        Ok(None)
    );
    assert_eq!(
        d.decode(&delete_path_ex(1, T0, 2, 3, 0x10, r"\x").raw()),
        Ok(None)
    );
    assert!(
        d.decode(&delete_path_ex(1, T0, 2, 3, 0x3, r"\x").raw())
            .unwrap()
            .is_some()
    );
}

#[test]
fn version_zero_layouts_decode() {
    // Create v0 has a pointer-sized ThreadId before FileObject.
    let p = Payload::new(8)
        .ptr(1)
        .ptr(2)
        .ptr(0xF0)
        .u32(0)
        .u32(0)
        .u32(0)
        .wstr(r"\a");
    let mut e = create(1, T0, 0, 0, "");
    e.version = 0;
    e.data = p.bytes;
    match Decoder::default().decode(&e.raw()).unwrap().unwrap().kind {
        EventKind::Create { file_object, .. } => assert_eq!(file_object, 0xF0),
        k => panic!("{k:?}"),
    }
}

#[test]
fn malformed_payloads_are_counted_not_fatal() {
    let (mut t, _) = tracker_at(T0, Snapshot::default());
    let w = write(10, T0, 1, 2, 3);
    for n in 0..w.data.len() {
        let mut cut = w.clone();
        cut.data.truncate(n);
        t.process_raw(&cut.raw());
    }
    let mut unknown = w.clone();
    unknown.version = 9;
    t.process_raw(&unknown.raw());
    // Garbage of every length through every decoded event id.
    for id in [10u16, 11, 12, 16, 26, 27, 30] {
        for n in 0..64usize {
            let mut g = w.clone();
            g.id = id;
            g.data = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            t.process_raw(&g.raw());
        }
    }
    let s = t.stats();
    // Every cut before the trailing ExtraFlags field, plus the unknown version.
    assert!(s.decode_errors > w.data.len() as u64 - 4);
    assert!(t.last_decode_error().is_some());
    assert!(t.top_writers(Window::Now, 10).is_empty());
}

// -----------------------------------------------------------------------------
// Mapping
// -----------------------------------------------------------------------------

#[test]
fn pid_reuse_attributes_to_the_right_process() {
    let (mut t, _) = tracker_at(T0 + 120, Snapshot::default());
    let first = r"\Device\HarddiskVolume3\Apps\first.exe";
    let second = r"\Device\HarddiskVolume3\Apps\second.exe";
    feed(
        &mut t,
        &[
            proc_start(500, T0, first),
            create(
                500,
                T0 + 1,
                0x10,
                0,
                r"\Device\HarddiskVolume3\Data\one.bin",
            ),
            write(500, T0 + 2, 0x10, 0, 1000),
            proc_stop(500, T0, T0 + 3),
            // Same pid, new process, same file object pointer recycled.
            proc_start(500, T0 + 60, second),
            create(
                500,
                T0 + 61,
                0x10,
                0,
                r"\Device\HarddiskVolume3\Data\two.bin",
            ),
            write(500, T0 + 62, 0x10, 0, 3000),
            // A write from pid 500 between the two processes (after the
            // grace period) is not blamed on either.
            write(500, T0 + 30, 0x10, 0, 7),
        ],
    );
    let top = t.top_writers(Window::LastHour, 10);
    let get = |img: &str| {
        top.iter()
            .find(|w| w.image == img)
            .map(|w| w.counts.bytes_written)
    };
    assert_eq!(get(r"C:\Apps\first.exe"), Some(1000));
    assert_eq!(get(r"C:\Apps\second.exe"), Some(3000));
    assert_eq!(t.stats().unattributed, 1);
}

#[test]
fn processes_running_before_start_come_from_the_snapshot() {
    let snap = Snapshot(vec![(42, r"C:\Pre\pre.exe".into(), ft(T0 - 1000))]);
    let (mut t, _) = tracker_at(T0 + 60, snap);
    feed(
        &mut t,
        &[
            create(42, T0, 0x1, 0, r"\Device\HarddiskVolume3\Pre\log.txt"),
            write(42, T0 + 1, 0x1, 0, 10),
        ],
    );
    assert_eq!(t.top_writers(Window::Now, 1)[0].image, r"C:\Pre\pre.exe");
}

#[test]
fn file_object_reused_for_different_names() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create(7, T0, 0x99, 0, r"\Device\HarddiskVolume3\A\a.txt"),
            write(7, T0, 0x99, 0, 100),
            create(7, T0 + 1, 0x99, 0, r"\Device\HarddiskVolume5\B\b.txt"),
            write(7, T0 + 1, 0x99, 0, 200),
        ],
    );
    let dirs = t.dir_totals(Window::Now, 10);
    let get = |d: &str| {
        dirs.iter()
            .find(|x| x.dir == d)
            .map(|x| x.counts.bytes_written)
    };
    assert_eq!(get(r"C:\A"), Some(100));
    assert_eq!(get(r"D:\B"), Some(200));
}

#[test]
fn file_key_names_survive_handle_churn_and_release() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            name_create(T0, 0xC0, r"\Device\HarddiskVolume3\K\keyed.db"),
            // No Create seen for this handle (opened before tracking).
            write(7, T0 + 1, 0xDEAD, 0xC0, 50),
            name_delete(T0 + 2, 0xC0, r"\Device\HarddiskVolume3\K\keyed.db"),
            write(7, T0 + 3, 0xDEAD, 0xC0, 60),
        ],
    );
    assert_eq!(t.dir_totals(Window::Now, 10)[0].counts.bytes_written, 50);
    assert_eq!(t.stats().unmapped_writes, 1);
}

#[test]
fn device_paths_normalize_to_drive_letters_and_unknown_devices_stay_nt() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create(7, T0, 1, 0, r"\Device\HarddiskVolume5\Models\m.gguf"),
            write(7, T0, 1, 0, 10),
            create(7, T0, 2, 0, r"\Device\HarddiskVolume99\X\y"),
            write(7, T0, 2, 0, 5),
        ],
    );
    let mut dirs: Vec<String> = t
        .dir_totals(Window::Now, 10)
        .into_iter()
        .map(|d| d.dir)
        .collect();
    dirs.sort();
    assert_eq!(dirs, [r"D:\Models", r"\Device\HarddiskVolume99\X"]);
    let batch = t.flush();
    assert!(
        batch
            .samples
            .iter()
            .any(|s| s.dir_hash == path_hash(r"D:\Models"))
    );
    assert!(batch.samples.iter().all(|s| s.image == APP_DOS));
}

// -----------------------------------------------------------------------------
// Counting rules
// -----------------------------------------------------------------------------

#[test]
fn renames_move_the_last_writer_and_later_writes() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    let tmp = r"\Device\HarddiskVolume3\Dl\file.part";
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create_new(7, T0, 0x5, tmp),
            name_create(T0, 0x6, tmp),
            write(7, T0 + 1, 0x5, 0x6, 1 << 20),
            rename_path(
                7,
                T0 + 2,
                0x5,
                0x6,
                r"\Device\HarddiskVolume3\Final\file.bin",
            ),
            write(7, T0 + 3, 0x5, 0x6, 10),
        ],
    );
    let batch = t.flush();
    let hashes: Vec<u64> = batch.last_writes.iter().map(|w| w.path_hash).collect();
    assert_eq!(hashes, [path_hash(r"C:\Final\file.bin")]);
    assert_eq!(batch.last_writes[0].pid, Some(7));
    assert_eq!(batch.last_writes[0].at, Timestamp(T0 + 3));
    let by_dir = |d: &str| {
        batch
            .samples
            .iter()
            .find(|s| s.dir_hash == path_hash(d))
            .map(|s| (s.bytes_written, s.files_created))
    };
    assert_eq!(by_dir(r"C:\Dl"), Some((1 << 20, 1)));
    assert_eq!(by_dir(r"C:\Final"), Some((10, 0)));
}

#[test]
fn rename_reporting_the_old_name_waits_for_name_create() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    let old = r"\Device\HarddiskVolume3\R\old.txt";
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create(7, T0, 0x5, 0, old),
            name_create(T0, 0x6, old),
            rename_path(7, T0 + 1, 0x5, 0x6, old),
            name_create(T0 + 1, 0x6, r"\Device\HarddiskVolume3\S\new.txt"),
            write(7, T0 + 2, 0x5, 0x6, 33),
        ],
    );
    let d = t.dir_totals(Window::Now, 10);
    assert_eq!(
        (d[0].dir.as_str(), d[0].counts.bytes_written),
        (r"C:\S", 33)
    );
}

#[test]
fn relative_rename_paths_do_not_remap() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create(7, T0, 0x5, 0, r"\Device\HarddiskVolume3\R\a.txt"),
            rename_path(7, T0 + 1, 0x5, 0, "b.txt"),
            write(7, T0 + 2, 0x5, 0, 9),
        ],
    );
    assert_eq!(t.dir_totals(Window::Now, 10)[0].dir, r"C:\R");
}

#[test]
fn deletes_creates_and_paging_writes() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    let f = r"\Device\HarddiskVolume3\T\x.tmp";
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create_new(7, T0, 1, f),
            write(7, T0, 1, 0, 4096),
            // Lazy-writer flush of the same data from System: not counted.
            write_ptr(4, T0 + 1, 1, 0, 4096, 0x2, 8),
            write_ptr(4, T0 + 1, 1, 0, 4096, 0x43, 8),
            delete_path(7, T0 + 2, 1, 0, true, f),
            delete_path(7, T0 + 2, 1, 0, false, f),
            // Delete-on-close open of another file.
            create(7, T0 + 3, 2, 0x1000, r"\Device\HarddiskVolume3\T\y.tmp"),
            // DeletePath with a volume-relative path resolves via the mapping.
            create(7, T0 + 4, 3, 0, r"\Device\HarddiskVolume5\U\z.tmp"),
            delete_path_ex(7, T0 + 4, 3, 0, 0x13, "z.tmp"),
        ],
    );
    let d = t.dir_totals(Window::Now, 10);
    let get = |p: &str| d.iter().find(|x| x.dir == p).unwrap().counts;
    assert_eq!(
        get(r"C:\T"),
        Counts {
            bytes_written: 4096,
            files_created: 1,
            files_deleted: 2
        }
    );
    assert_eq!(get(r"D:\U").files_deleted, 1);
    assert_eq!(t.stats().paging_writes, 2);
    assert!(
        t.top_writers(Window::Now, 10)
            .iter()
            .all(|w| w.image == APP_DOS)
    );
}

// -----------------------------------------------------------------------------
// Windows and rollups
// -----------------------------------------------------------------------------

#[test]
fn rollups_bucket_across_hour_boundaries() {
    let (mut t, clock) = tracker_at(T0 + 3600 + 60, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create(7, T0, 1, 0, r"\Device\HarddiskVolume3\H\f"),
            write(7, T0 + 3599, 1, 0, 100),
            write(7, T0 + 3600, 1, 0, 200),
            write(7, T0 + 3600 + 59, 1, 0, 400),
        ],
    );
    let batch = t.flush();
    let mut hours: Vec<(i64, u64)> = batch
        .samples
        .iter()
        .map(|s| (s.at.0, s.bytes_written))
        .collect();
    hours.sort_unstable();
    assert_eq!(hours, [(T0, 100), (T0 + 3600, 600)]);
    // Draining empties the pending rows; the windows keep their data.
    assert!(t.flush().is_empty());

    // Now = current + previous minute.
    assert_eq!(t.top_writers(Window::Now, 1)[0].counts.bytes_written, 600);
    assert_eq!(
        t.top_writers(Window::LastHour, 1)[0].counts.bytes_written,
        700
    );
    let since = Window::Since(Timestamp(T0 + 3600));
    assert_eq!(t.top_writers(since, 1)[0].counts.bytes_written, 600);
    assert_eq!(
        t.top_writers(Window::Since(Timestamp(T0 - 5)), 1)[0]
            .counts
            .bytes_written,
        700
    );

    clock.advance_secs(120);
    assert!(t.top_writers(Window::Now, 1).is_empty());
    clock.advance_secs(3600);
    t.flush();
    assert!(t.top_writers(Window::LastHour, 1).is_empty());
    assert_eq!(
        t.top_writers(Window::Since(Timestamp(T0)), 1)[0]
            .counts
            .bytes_written,
        700
    );
    // Hour buckets are kept for 48 hours.
    clock.advance_secs(48 * 3600);
    t.flush();
    assert!(t.top_writers(Window::Since(Timestamp(0)), 1).is_empty());
}

#[test]
fn late_events_land_in_their_own_hour() {
    let (mut t, _) = tracker_at(T0 + 7200, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create(7, T0, 1, 0, r"\Device\HarddiskVolume3\L\f"),
            write(7, T0 + 7100, 1, 0, 1),
            write(7, T0 + 10, 1, 0, 2),
        ],
    );
    let mut hours: Vec<i64> = t.flush().samples.iter().map(|s| s.at.0).collect();
    hours.sort_unstable();
    assert_eq!(hours, [T0, T0 + 3600]);
}

#[test]
fn sampling_scales_writes_and_never_drops_metadata_events() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    t.set_sample_rate(4);
    let mut evs = vec![proc_start(7, T0, APP)];
    for i in 0..100u64 {
        evs.push(create_new(
            7,
            T0,
            0x100 + i,
            &format!(r"\Device\HarddiskVolume3\S\f{i}"),
        ));
        evs.push(write(7, T0, 0x100 + i, 0, 1000));
    }
    feed(&mut t, &evs);
    let c = t.dir_totals(Window::Now, 1)[0].counts;
    assert_eq!(c.files_created, 100);
    assert_eq!(c.bytes_written, 100_000);
    assert_eq!(t.stats().sampled_out, 75);
    assert_eq!(t.sample_rate(), 4);
}

#[test]
fn evidence_weights_from_write_share() {
    let (mut t, _) = tracker_at(T0 + 4 * 3600, Snapshot::default());
    let other = r"\Device\HarddiskVolume3\Other\other.exe";
    let mut evs = vec![proc_start(7, T0, APP), proc_start(8, T0, other)];
    evs.push(create(
        7,
        T0,
        1,
        0,
        r"\Device\HarddiskVolume3\Users\me\AppData\Local\Tool\cache\a",
    ));
    evs.push(create(
        8,
        T0,
        2,
        0,
        r"\Device\HarddiskVolume3\Users\me\AppData\Local\Tool\cache\b",
    ));
    evs.push(create(8, T0, 3, 0, r"\Device\HarddiskVolume3\Shared\x"));
    evs.push(create(7, T0, 4, 0, r"\Device\HarddiskVolume3\Shared\y"));
    for h in 0..4 {
        let at = T0 + h * 3600 + 5;
        evs.push(write(7, at, 1, 0, 900_000));
        evs.push(write(8, at, 2, 0, 100_000));
        evs.push(write(8, at, 3, 0, 500_000));
        evs.push(write(7, at, 4, 0, 500_000));
    }
    feed(&mut t, &evs);
    let ev = t.evidence(Timestamp(T0), &EvidenceConfig::default());
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].prefix, r"C:\Users\me\AppData\Local\Tool\cache");
    assert_eq!(ev[0].app, "tool.exe");
    assert!((ev[0].share - 0.9).abs() < 1e-4);
    assert!((ev[0].weight - 0.9).abs() < 1e-4);
    // Only the last hour: one active hour → a third of the support.
    let ev = t.evidence(Timestamp(T0 + 3 * 3600), &EvidenceConfig::default());
    assert!((ev[0].weight - 0.3).abs() < 1e-4);
}

#[test]
fn clear_forgets_activity_but_keeps_name_maps() {
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create(7, T0, 1, 0, r"\Device\HarddiskVolume3\C\f"),
            write(7, T0, 1, 0, 5),
        ],
    );
    t.clear();
    assert!(t.flush().is_empty());
    assert!(t.top_writers(Window::LastHour, 10).is_empty());
    assert_eq!(t.stats(), Default::default());
    feed(&mut t, &[write(7, T0 + 1, 1, 0, 6)]);
    assert_eq!(t.top_writers(Window::Now, 1)[0].counts.bytes_written, 6);
}

#[test]
fn batches_round_trip_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = strata_store::Store::open(dir.path()).unwrap();
    let (mut t, _) = tracker_at(T0 + 60, Snapshot::default());
    feed(
        &mut t,
        &[
            proc_start(7, T0, APP),
            create_new(7, T0, 1, r"\Device\HarddiskVolume3\Store\f.bin"),
            write(7, T0 + 1, 1, 0, 1234),
        ],
    );
    let b = t.flush();
    store.record_activity(&b.samples).unwrap();
    store.set_last_writers(&b.last_writes).unwrap();
    let top = store.top_writers(Timestamp(T0), 5).unwrap();
    assert_eq!(
        (
            top[0].image.as_str(),
            top[0].bytes_written,
            top[0].files_created
        ),
        (APP_DOS, 1234, 1)
    );
    let dw = store
        .dir_writers(path_hash(r"C:\Store"), Timestamp(T0), 5)
        .unwrap();
    assert_eq!(dw.len(), 1);
    let lw = store
        .last_writer(path_hash(r"c:\store\F.BIN"))
        .unwrap()
        .unwrap();
    assert_eq!(
        (lw.image.as_str(), lw.pid, lw.at),
        (APP_DOS, Some(7), Timestamp(T0 + 1))
    );
    // Batches serialize for the helper → app pipe.
    let json = serde_json::to_string(&b).unwrap();
    assert_eq!(
        serde_json::from_str::<strata_etw::ActivityBatch>(&json).unwrap(),
        b
    );
}
