//! Runs the `strata-cli` binary against image files written by the
//! synthetic builder.

use std::path::{Path, PathBuf};
use std::process::Command;

use strata_cli::golden::Golden;
use strata_core::{FileRef, win32};
use strata_ntfs::NS_WIN32;
use strata_ntfs::test_image::{
    DEFAULT_TIMES, Geometry, ImageBuilder, NonResidentSpec, ROOT, RecordBuilder, raw_reparse,
    symlink_reparse,
};

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// A small volume: nested directories, a large file, a hardlink, an ADS, a
/// symlink, a cloud placeholder and an orphan.
fn write_image(name: &str) -> PathBuf {
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .mft_fragments(4)
        .mft_data_in_extension(15);
    let big = b.alloc(100);
    let mid = b.alloc(3);
    let users = FileRef::from_parts(64, 1);
    let docs = FileRef::from_parts(65, 1);
    b.insert(64, RecordBuilder::dir(1, ROOT, "Users"));
    b.insert(65, RecordBuilder::dir(1, users, "docs"));
    b.insert(
        66,
        RecordBuilder::file(1, docs, "big.iso").data_nonresident(
            "",
            0,
            NonResidentSpec::new(big, 400_000, 4096),
        ),
    );
    b.insert(
        67,
        RecordBuilder::file(1, docs, "report.pdf")
            .name(users, "report-link.pdf", NS_WIN32)
            .data_nonresident("", 0, NonResidentSpec::new(mid, 12_000, 4096))
            .data("Zone.Identifier", b"[ZoneTransfer]\r\nZoneId=3\r\n"),
    );
    b.insert(
        68,
        RecordBuilder::file(1, users, "shortcut")
            .data("", &[])
            .reparse(symlink_reparse(
                r"\??\C:\Users\docs",
                r"C:\Users\docs",
                false,
            )),
    );
    b.insert(
        69,
        RecordBuilder::new(1)
            .std_info(DEFAULT_TIMES, win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            .name(docs, "cloud.docx", NS_WIN32)
            .data("", &[])
            .reparse(raw_reparse(0x9000_701A, &[0; 8])),
    );
    b.insert(
        70,
        RecordBuilder::file(1, FileRef::from_parts(500, 1), "lost.txt").data("", b"?"),
    );
    let path = tmp(name);
    std::fs::write(&path, b.finish()).unwrap();
    path
}

fn cli(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_strata-cli"))
        .args(args)
        .output()
        .expect("binary runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn scans_an_image_and_reports() {
    let img = write_image("cli-report.img");
    let (code, out, err) = cli(&["scan", img.to_str().unwrap(), "--top", "5"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("Largest 5 files by allocated size"), "{out}");
    let largest: Vec<&str> = out
        .lines()
        .skip_while(|l| !l.starts_with("Largest"))
        .skip(1)
        .take(5)
        .collect();
    assert!(largest[0].ends_with(r"\Users\docs\big.iso"), "{largest:?}");
    assert!(
        largest
            .iter()
            .any(|l| l.contains(r"\Users\docs\report.pdf  (+1 more links)"))
    );
    assert!(out.contains("used ($Bitmap)"), "{out}");
    // Every used cluster of a builder image belongs to a record.
    let gap = out
        .lines()
        .find(|l| l.trim_start().starts_with("Unaccounted"))
        .unwrap();
    assert!(gap.ends_with("(0 bytes)"), "{gap}");
    assert!(out.contains("none: every used cluster is attributed to a record"));
    assert!(
        out.contains("extension records            1 seen, 1 merged, 0 orphaned"),
        "{out}"
    );
}

#[test]
fn writes_golden_json() {
    let img = write_image("cli-golden.img");
    let json = tmp("cli-golden.json");
    for mode in ["--sequential", "--no-buffering"] {
        let (code, out, err) = cli(&[
            "scan",
            img.to_str().unwrap(),
            "--json",
            json.to_str().unwrap(),
            "--chunk-mib",
            "1",
            mode,
            "--mft-bitmap",
        ]);
        assert_eq!(code, 0, "{mode} stderr: {err}\n{out}");
        let doc: Golden = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        assert_eq!(doc.format, "strata-golden/1");
        assert_eq!(doc.volume.cluster_size, 4096);
        let get = |p: &str| {
            doc.entries
                .iter()
                .find(|e| e.path == p)
                .unwrap_or_else(|| panic!("no entry {p}"))
        };
        let root = get(r"\");
        assert_eq!((root.record, root.kind.as_str()), (5, "dir"));
        let big = get(r"\Users\docs\big.iso");
        assert_eq!((big.logical, big.allocated), (400_000, 100 * 4096));
        let a = get(r"\Users\docs\report.pdf");
        let b = get(r"\Users\report-link.pdf");
        assert_eq!((a.record, a.link_index, a.link_count), (67, 0, 2));
        assert_eq!((b.record, b.link_index), (67, 1));
        assert_eq!(a.ads.len(), 1);
        assert!(a.flags.contains(&"has-ads".to_owned()));
        let link = get(r"\Users\shortcut");
        let rp = link.reparse.as_ref().unwrap();
        assert_eq!(
            (rp.kind.as_str(), rp.target.as_deref()),
            ("symlink", Some(r"C:\Users\docs"))
        );
        assert_eq!(
            get(r"\Users\docs\cloud.docx").cloud.as_deref(),
            Some("online-only")
        );
        assert_eq!(get(r"<orphan>\lost.txt").record, 70);
        assert!(get(r"\$MFT").flags.contains(&"ntfs-metadata".to_owned()));
        let mut sorted = doc.entries.clone();
        sorted.sort_by(|x, y| (&x.path, x.link_index).cmp(&(&y.path, y.link_index)));
        assert_eq!(sorted, doc.entries);
        assert_eq!(
            doc.volume.used_bytes,
            Some(doc.totals.sum_allocated() + doc.volume.other_attr_allocated)
        );
    }
}

#[test]
fn drive_without_elevation_exits_2() {
    if strata_cli::is_elevated().unwrap_or(true) {
        return;
    }
    let (code, _, err) = cli(&["scan", "C:"]);
    assert_eq!(code, 2);
    assert!(err.contains("Run from an elevated terminal: MFT scanning needs administrator rights"));
}

#[test]
fn usage_and_open_errors() {
    let (code, out, _) = cli(&["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("Usage: strata-cli scan"));
    let (code, _, err) = cli(&["scan", "a.img", "--top"]);
    assert_eq!(code, 64);
    assert!(err.contains("--top needs a value"));
    let (code, _, err) = cli(&["scan", tmp("does-not-exist.img").to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("cannot open"));
    let junk = tmp("cli-junk.img");
    std::fs::write(&junk, vec![0u8; 8192]).unwrap();
    let (code, _, err) = cli(&["scan", junk.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("invalid NTFS boot sector"), "{err}");
}
