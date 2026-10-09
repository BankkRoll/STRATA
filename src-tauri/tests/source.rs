//! `IndexSource` (the `LayoutSource` over the index): sizes per mode,
//! filtered children, wire ids and the packed `color_key`.

use strata_app_lib::classify::{Engine, classify_index, ext_slots};
use strata_app_lib::ids;
use strata_app_lib::layout_pipe::IndexSource;
use strata_app_lib::model::{ScannerUsed, ViewFilters, VolumeData};
use strata_core::known::KnownFolders;
use strata_core::{
    Category, EntryFlags, EpochSecs, FileRef, FileTime, NameLink, ScanRecord, SizeMode, Sizes,
    Times, WideName,
};
use strata_index::{IndexBuilder, IndexOptions};
use strata_layout::LayoutSource;

const NOW: u32 = 800_000_000;

fn rec(
    id: u64,
    parent: u64,
    name: &str,
    logical: u64,
    allocated: u64,
    dir: bool,
    age: u32,
) -> ScanRecord {
    let r = |n| FileRef::from_parts(n, 1);
    let modified = EpochSecs(NOW - age).to_filetime();
    ScanRecord {
        id: r(id),
        links: vec![NameLink {
            parent: r(parent),
            name: WideName::from_str_lossless(name),
        }],
        attributes: 0,
        flags: if dir {
            EntryFlags::DIR
        } else {
            EntryFlags::EMPTY
        },
        times: Times {
            created: modified,
            modified,
            accessed: modified,
            changed: FileTime(0),
        },
        fn_created: None,
        sizes: Sizes {
            logical,
            allocated,
            ..Sizes::default()
        },
        reparse: None,
        ads: vec![],
    }
}

fn volume() -> (Engine, VolumeData) {
    let mut b = IndexBuilder::new(IndexOptions {
        now: EpochSecs(NOW).to_filetime(),
        ..IndexOptions::default()
    });
    b.push_batch([
        rec(5, 5, "", 0, 0, true, 0),
        rec(20, 5, "app", 0, 0, true, 30 * 86_400),
        rec(21, 20, "package.json", 10, 4096, false, 100),
        rec(22, 20, "node_modules", 0, 0, true, 30 * 86_400),
        rec(23, 22, "lib.js", 5000, 8192, false, 2 * 86_400),
        rec(
            30,
            5,
            "movie.mp4",
            1_000_000,
            1_003_520,
            false,
            400 * 86_400,
        ),
        rec(31, 5, "tiny.txt", 3, 0, false, 0),
    ])
    .unwrap();
    let mut index = b.finish().unwrap();
    let engine = Engine::new(&KnownFolders::default(), None).unwrap();
    let slots = ext_slots(&index);
    let c = classify_index(&engine, &mut index, r"D:\data", &slots);
    let data = VolumeData::new(1, index, c, r"D:\data".into(), ScannerUsed::Walker);
    (engine, data)
}

fn child(src: &IndexSource<'_>, parent: u32, data: &VolumeData, name: &str) -> (u32, u64) {
    let mut out = Vec::new();
    src.children(parent, &mut out);
    *out.iter()
        .find(|(id, _)| data.index.name_lossy(data.resolve(*id).unwrap()) == name)
        .unwrap_or_else(|| panic!("{name} missing"))
}

#[test]
fn sizes_follow_the_mode() {
    let (_, data) = volume();
    let root = data.root_wire();
    let alloc = IndexSource::new(&data, SizeMode::Allocated, &ViewFilters::default());
    let logical = IndexSource::new(&data, SizeMode::Logical, &ViewFilters::default());
    assert_eq!(alloc.size(root), 4096 + 8192 + 1_003_520);
    assert_eq!(logical.size(root), 10 + 5000 + 1_000_000 + 3);
    let (app, a) = child(&alloc, root, &data, "app");
    assert_eq!(a, 4096 + 8192);
    assert_eq!(child(&logical, root, &data, "app").1, 5010);
    assert!(alloc.is_dir(app));
    assert_eq!(ids::decode(app).0, 1, "children carry the generation");
    let mut out = Vec::new();
    alloc.children(root, &mut out);
    assert_eq!(out.len(), 3, "empty virtual group nodes are hidden");
}

#[test]
fn color_key_packs_every_mode() {
    let (_, data) = volume();
    let src = IndexSource::new(&data, SizeMode::Allocated, &ViewFilters::default()).with_now(NOW);
    let root = data.root_wire();
    let (app, _) = child(&src, root, &data, "app");
    let (nm, _) = child(&src, app, &data, "node_modules");
    let (lib, _) = child(&src, nm, &data, "lib.js");
    let (mp4, _) = child(&src, root, &data, "movie.mp4");

    let k = src.color_key(nm);
    assert_eq!(k & 0xF, Category::DevBuild as u32, "category");
    assert_eq!((k >> 4) & 7, 1, "safe");
    assert_eq!(
        (k >> 7) & 0x1F,
        4,
        "subtree newest is 2 days old: bucket <3 d"
    );
    assert_eq!((k >> 12) & 0xFF, 0, "directories have no type slot");
    assert_ne!((k >> 20) & 0x3FF, 0, "attributed to the rule's app");
    assert_eq!(k >> 30, 0, "no live change, reserved bit clear");

    let f = src.color_key(lib);
    assert_eq!(f & 0xF, Category::DevBuild as u32);
    assert_ne!((f >> 12) & 0xFF, 0, "files get a type slot");

    let m = src.color_key(mp4);
    assert_eq!((m >> 7) & 0x1F, 13, "400 days: bucket <548 d");
    assert_eq!((m >> 12) & 0xFF, 1, "the largest extension gets slot 1");

    // The app folder has no rule: it takes the dominant (largest) child's
    // category.
    assert_eq!(src.color_key(app) & 0xF, Category::DevBuild as u32);
}

#[test]
fn filters_hide_children() {
    let (_, data) = volume();
    let root = data.root_wire();
    let names = |f: &ViewFilters| {
        let src = IndexSource::new(&data, SizeMode::Logical, f);
        let mut out = Vec::new();
        src.children(root, &mut out);
        let mut n: Vec<String> = out
            .iter()
            .map(|(id, _)| data.index.name_lossy(data.resolve(*id).unwrap()))
            .collect();
        n.sort();
        n
    };
    assert_eq!(
        names(&ViewFilters {
            min_bytes: 100,
            ..ViewFilters::default()
        }),
        ["app", "movie.mp4"]
    );
    let mp4 = data.wire(
        (0..data.index.slot_count() as u32)
            .map(strata_index::EntryId)
            .find(|&e| data.index.is_live(e) && data.index.name_lossy(e) == "movie.mp4")
            .unwrap(),
    );
    assert_eq!(
        names(&ViewFilters {
            excluded: vec![mp4],
            ..ViewFilters::default()
        }),
        ["app", "tiny.txt"]
    );
    assert_eq!(
        names(&ViewFilters {
            categories: vec![Category::DevBuild as u16],
            ..ViewFilters::default()
        }),
        ["app"]
    );
}
