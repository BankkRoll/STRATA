//! Read queries: sorted pages, paths, top-N, filters, breakdowns.

mod common;

use common::*;
use strata_core::{CloudState, EntryFlags, EpochSecs, FileTime, SizeMode, win32};
use strata_index::{ChildQuery, EntryId, EntryKind, Filter, Index, SortKey};

fn tree() -> Index {
    let mut recs = vec![
        root_rec(),
        dir(r(30, 1), ROOT, "Media"),
        dir(r(31, 1), ROOT, "src"),
        dir(r(32, 1), r(31, 1), "node_modules"),
        dir(r(33, 1), r(32, 1), "react"),
        file(r(40, 1), r(30, 1), "clip10.mp4", 5_000_000),
        file(r(41, 1), r(30, 1), "clip2.MP4", 9_000_000),
        file(r(42, 1), r(30, 1), "Clip1.mkv", 1_000),
        file(r(43, 1), r(33, 1), "index.js", 12_000),
        file(r(44, 1), r(31, 1), "main.rs", 3_000),
    ];
    recs[6].times = times(1_750_000_000);
    let mut cloud = file(r(45, 1), r(30, 1), "online.mov", 0);
    cloud.sizes.logical = 2_000_000_000;
    cloud.sizes.allocated = 0;
    cloud.attributes = win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    cloud.reparse = Some(strata_core::Reparse {
        tag: win32::IO_REPARSE_TAG_CLOUD,
        target: None,
    });
    recs.push(cloud);
    let mut hidden = file(r(46, 1), r(31, 1), ".env", 10);
    hidden.flags |= EntryFlags::HIDDEN;
    recs.push(hidden);
    build(
        recs,
        strata_index::IndexOptions {
            volume: strata_index::VolumeInfo {
                prefix: "C:".into(),
                ..Default::default()
            },
            ..opts()
        },
    )
}

fn names(idx: &Index, ids: &[EntryId]) -> Vec<String> {
    ids.iter().map(|&i| idx.name_lossy(i)).collect()
}

#[test]
fn children_sorted_by_each_key() {
    let idx = tree();
    let media = idx.lookup(r(30, 1)).unwrap();
    let q = |key, descending, mode| ChildQuery {
        key,
        descending,
        mode,
        ..ChildQuery::default()
    };
    let by_size = idx.children_sorted(media, &q(SortKey::Size, true, SizeMode::Allocated));
    assert_eq!(
        names(&idx, &by_size),
        ["clip2.MP4", "clip10.mp4", "Clip1.mkv", "online.mov"]
    );
    let by_logical = idx.children_sorted(media, &q(SortKey::Size, true, SizeMode::Logical));
    assert_eq!(idx.name_lossy(by_logical[0]), "online.mov");
    let by_name = idx.children_sorted(media, &q(SortKey::Name, false, SizeMode::Allocated));
    assert_eq!(
        names(&idx, &by_name),
        ["Clip1.mkv", "clip2.MP4", "clip10.mp4", "online.mov"]
    );
    let by_time = idx.children_sorted(media, &q(SortKey::Modified, true, SizeMode::Allocated));
    assert_eq!(idx.name_lossy(by_time[0]), "clip2.MP4");

    let root = idx.root();
    let page = idx.children_sorted(
        root,
        &ChildQuery {
            key: SortKey::Count,
            dirs_first: true,
            offset: 0,
            limit: 2,
            ..ChildQuery::default()
        },
    );
    assert_eq!(names(&idx, &page), ["src", "Media"]);
    let page2 = idx.children_sorted(
        root,
        &ChildQuery {
            key: SortKey::Count,
            dirs_first: true,
            offset: 2,
            limit: 10,
            ..ChildQuery::default()
        },
    );
    assert_eq!(page2.len(), 2);
}

#[test]
fn large_directory_pages_are_consistent() {
    let mut recs = vec![root_rec(), dir(r(30, 1), ROOT, "big")];
    for i in 0..5_000u64 {
        recs.push(file(
            r(100 + i, 1),
            r(30, 1),
            &format!("f{i}"),
            (i * 7919) % 10_007,
        ));
    }
    let idx = build(recs, opts());
    let big = idx.lookup(r(30, 1)).unwrap();
    let all = idx.children_sorted(big, &ChildQuery::default());
    let mut pages = Vec::new();
    for p in 0..50 {
        pages.extend(idx.children_sorted(
            big,
            &ChildQuery {
                offset: p * 100,
                limit: 100,
                ..ChildQuery::default()
            },
        ));
    }
    assert_eq!(all, pages);
    let sizes: Vec<u64> = all
        .iter()
        .map(|&e| idx.size(e, SizeMode::Allocated))
        .collect();
    assert!(sizes.windows(2).all(|w| w[0] >= w[1]));
}

#[test]
fn paths_and_cache_invalidation() {
    let mut idx = tree();
    let js = idx.lookup(r(43, 1)).unwrap();
    assert_eq!(idx.path_string(js), r"C:\src\node_modules\react\index.js");
    assert_eq!(idx.path_string(idx.root()), r"C:\");
    // Cached ancestors are reused.
    let react = idx.lookup(r(33, 1)).unwrap();
    assert_eq!(idx.path_string(react), r"C:\src\node_modules\react");
    idx.upsert(dir(r(31, 1), ROOT, "source")).unwrap();
    assert_eq!(
        idx.path_string(js),
        r"C:\source\node_modules\react\index.js"
    );
}

#[test]
fn top_n_files_and_dirs() {
    let idx = tree();
    let top = idx.top_n(None, 2, EntryKind::Files, SizeMode::Allocated, None);
    assert_eq!(
        names(&idx, &top.iter().map(|t| t.0).collect::<Vec<_>>()),
        ["clip2.MP4", "clip10.mp4"]
    );
    assert_eq!(top[0].1, 9_000_000u64.next_multiple_of(4096));
    let src = idx.lookup(r(31, 1)).unwrap();
    let under = idx.top_n(Some(src), 10, EntryKind::Files, SizeMode::Allocated, None);
    assert_eq!(under.len(), 3);
    let dirs = idx.top_n(None, 3, EntryKind::Dirs, SizeMode::Allocated, None);
    assert_eq!(
        names(&idx, &dirs.iter().map(|t| t.0).collect::<Vec<_>>()),
        ["", "Media", "src"]
    );
    let only_js = Filter {
        exts: vec![idx.extension_id("JS").unwrap()],
        ..Filter::default()
    };
    let js = idx.top_n(None, 5, EntryKind::Any, SizeMode::Allocated, Some(&only_js));
    assert_eq!(js.len(), 1);
}

#[test]
fn filters_combine() {
    let idx = tree();
    let f = Filter {
        kind: EntryKind::Files,
        exts: vec![idx.extension_id("mp4").unwrap()],
        size: Some((6_000_000, u64::MAX)),
        size_mode: SizeMode::Logical,
        ..Filter::default()
    };
    assert_eq!(
        names(&idx, &idx.filter_entries(None, &f, 10)),
        ["clip2.MP4"]
    );

    let cloud = Filter {
        cloud: vec![CloudState::OnlineOnly],
        ..Filter::default()
    };
    assert_eq!(
        names(&idx, &idx.filter_entries(None, &cloud, 10)),
        ["online.mov"]
    );

    let hidden = Filter {
        flags_all: EntryFlags::HIDDEN,
        ..Filter::default()
    };
    let src = idx.lookup(r(31, 1)).unwrap();
    assert_eq!(
        names(&idx, &idx.filter_entries(Some(src), &hidden, 10)),
        [".env"]
    );

    let t = EpochSecs::from_filetime(FileTime::from_unix_secs(1_740_000_000));
    let recent = Filter {
        kind: EntryKind::Files,
        modified: Some((t, EpochSecs(u32::MAX))),
        ..Filter::default()
    };
    assert_eq!(
        names(&idx, &idx.filter_entries(None, &recent, 10)),
        ["clip2.MP4"]
    );

    let mut idx = idx;
    let main = idx.lookup(r(44, 1)).unwrap();
    idx.set_category(main, strata_core::Category::DevBuild as u16);
    idx.set_owner_app(main, 7);
    let app = Filter {
        owner_apps: vec![7],
        categories: vec![strata_core::Category::DevBuild as u16],
        ..Filter::default()
    };
    assert_eq!(
        names(&idx, &idx.filter_entries(None, &app, 10)),
        ["main.rs"]
    );
    let virt = Filter {
        include_virtual: true,
        kind: EntryKind::Dirs,
        flags_all: EntryFlags::VIRTUAL,
        ..Filter::default()
    };
    assert_eq!(idx.filter_entries(None, &virt, 10).len(), 2);
}

#[test]
fn extension_and_category_breakdowns() {
    let mut idx = tree();
    let b = idx.extension_breakdown(None, SizeMode::Logical);
    assert_eq!(b[0].label, "mov");
    let mp4 = b.iter().find(|x| x.label == "mp4").unwrap();
    assert_eq!((mp4.files, mp4.bytes), (2, 14_000_000));
    let none = b.iter().find(|x| x.label.is_empty()).unwrap();
    assert_eq!(none.files, 1);
    let media = idx.lookup(r(30, 1)).unwrap();
    let under: u64 = idx
        .extension_breakdown(Some(media), SizeMode::Allocated)
        .iter()
        .map(|x| x.bytes)
        .sum();
    assert_eq!(under, idx.aggregate(media).unwrap().allocated);

    for c in idx.children(media).collect::<Vec<_>>() {
        idx.set_category(c, strata_core::Category::Media as u16);
    }
    let cats = idx.category_breakdown(None, SizeMode::Allocated);
    assert_eq!(cats[0].label, "Media");
    assert_eq!(cats[0].files, 4);
}
