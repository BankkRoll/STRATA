//! Search semantics: term kinds, filters, providers, streaming, cancel.

mod common;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use strata_core::{EntryFlags, FileTime, Safety, SizeMode, WideName};
use strata_index::search::{CancelToken, Hit, Query, SearchOptions, SearchSort};
use strata_index::{EntryId, Index, IndexOptions, VolumeInfo};

fn tree() -> Index {
    let mut recs = vec![
        root_rec(),
        dir(r(30, 1), ROOT, "Users"),
        dir(r(31, 1), r(30, 1), "me"),
        dir(r(32, 1), r(31, 1), "project"),
        dir(r(33, 1), r(32, 1), "node_modules"),
        dir(r(34, 1), r(33, 1), "react"),
        dir(r(35, 1), r(33, 1), "react-dom"),
        file(r(40, 1), r(34, 1), "index.js", 10),
        file(r(41, 1), r(31, 1), "Report-2024.pdf", 2_000_000),
        file(r(42, 1), r(31, 1), "report", 50),
        file(r(43, 1), r(31, 1), "myreport.docx", 3_000),
        file(r(44, 1), r(31, 1), "llama-3-8b.Q4_K_M.gguf", 4 << 30),
        file(r(45, 1), r(31, 1), "IMG_0001.JPG", 4_000_000),
        file(r(46, 1), r(31, 1), "IMG_12.jpg", 3_000_000),
        file(r(47, 1), r(31, 1), "ÄRGER.txt", 1),
        file(r(48, 1), r(32, 1), "react.md", 1),
    ];
    recs[12].times = times(1_759_000_000);
    recs.push(record(
        r(49, 1),
        vec![strata_core::NameLink {
            parent: r(31, 1),
            name: WideName::from_units(vec![u16::from(b'z'), 0xD800, u16::from(b'q')]),
        }],
        false,
        1,
        1,
    ));
    build(
        recs,
        IndexOptions {
            volume: VolumeInfo {
                prefix: "D:".into(),
                ..VolumeInfo::default()
            },
            ..opts()
        },
    )
}

fn run(idx: &Index, q: &str) -> Vec<String> {
    run_with(idx, q, &SearchOptions::default())
}

fn run_with(idx: &Index, q: &str, opts: &SearchOptions<'_>) -> Vec<String> {
    let q = Query::parse(q, now()).unwrap();
    let out = idx.search(&q, opts, &CancelToken::new(), &|_| {}).unwrap();
    out.hits.iter().map(|h| idx.name_lossy(h.id)).collect()
}

#[test]
fn substring_is_case_insensitive_and_ranked() {
    let idx = tree();
    assert_eq!(
        run(&idx, "REPORT"),
        ["report", "Report-2024.pdf", "myreport.docx"]
    );
    assert_eq!(run(&idx, "case:yes REPORT"), Vec::<String>::new());
    assert_eq!(run(&idx, "case:yes Report"), ["Report-2024.pdf"]);
    assert_eq!(run(&idx, "ärger"), ["ÄRGER.txt"]);
}

#[test]
fn prefix_wildcard_regex() {
    let idx = tree();
    assert_eq!(run(&idx, "^img"), ["IMG_0001.JPG", "IMG_12.jpg"]);
    assert_eq!(run(&idx, "*.gguf"), ["llama-3-8b.Q4_K_M.gguf"]);
    assert_eq!(run(&idx, "img_??.jpg"), ["IMG_12.jpg"]);
    assert_eq!(run(&idx, r"re:^img_\d{4}\."), ["IMG_0001.JPG"]);
    assert_eq!(run(&idx, r"/^report$/"), ["report"]);
    let q = Query::parse_regex(r"^img_\d+\.jpg$ size:>3mb", now()).unwrap();
    let out = idx
        .search(&q, &SearchOptions::default(), &CancelToken::new(), &|_| {})
        .unwrap();
    assert_eq!(out.hits.len(), 1);
    assert_eq!(idx.name_lossy(out.hits[0].id), "IMG_0001.JPG");
}

#[test]
fn path_components() {
    let idx = tree();
    assert_eq!(run(&idx, r"node_modules\react"), ["react"]);
    assert_eq!(run(&idx, "node_modules/react*"), ["react", "react-dom"]);
    assert_eq!(run(&idx, r"react\index.js"), ["index.js"]);
    assert!(run(&idx, r"me\react").is_empty());
}

#[test]
fn filters_and_terms_and_together() {
    let idx = tree();
    assert_eq!(run(&idx, "file: size:>1gb"), ["llama-3-8b.Q4_K_M.gguf"]);
    assert_eq!(
        run(&idx, "size:>1gb"),
        ["", "Users", "me", "llama-3-8b.Q4_K_M.gguf"]
    );
    assert_eq!(run(&idx, "ext:jpg size:<3.5mb"), ["IMG_12.jpg"]);
    assert_eq!(
        run(&idx, "ext:pdf,docx"),
        ["Report-2024.pdf", "myreport.docx"]
    );
    assert_eq!(run(&idx, "dir: react"), ["react", "react-dom"]);
    assert_eq!(run(&idx, "file: react"), ["react.md"]);
    assert_eq!(run(&idx, "file: modified:<30d"), ["IMG_0001.JPG"]);
    assert_eq!(run(&idx, "vol:D: *.gguf").len(), 1);
    assert!(run(&idx, "vol:C *.gguf").is_empty());
    assert!(run(&idx, "ext:nonexistent").is_empty());
    assert_eq!(run(&idx, "zq").len(), 0);
    assert_eq!(
        run(&idx, "z").len(),
        1,
        "names with unpaired surrogates are searchable"
    );
}

#[test]
fn providers_back_safe_and_app_filters() {
    let mut idx = tree();
    let gguf = idx.lookup(r(44, 1)).unwrap();
    idx.set_owner_app(gguf, 3);
    let safety = |id: EntryId| {
        if id == gguf {
            Safety::Careful
        } else {
            Safety::Safe
        }
    };
    let apps = |app: u32, q: &str| app == 3 && "LM Studio".to_lowercase().contains(q);
    let opts = SearchOptions {
        safety: Some(&safety),
        app_matches: Some(&apps),
        ..SearchOptions::default()
    };
    assert_eq!(
        run_with(&idx, "safe:careful", &opts),
        ["llama-3-8b.Q4_K_M.gguf"]
    );
    assert_eq!(run_with(&idx, "safe:no", &opts), ["llama-3-8b.Q4_K_M.gguf"]);
    assert_eq!(
        run_with(&idx, "app:studio", &opts),
        ["llama-3-8b.Q4_K_M.gguf"]
    );
    assert!(run_with(&idx, "app:claude", &opts).is_empty());
    // Without providers these filters cannot match.
    assert!(run(&idx, "safe:yes").is_empty());
}

#[test]
fn size_sort_limit_and_streaming() {
    let idx = tree();
    let q = Query::parse("i", now()).unwrap();
    let batches = AtomicUsize::new(0);
    let streamed = Mutex::new(Vec::<Hit>::new());
    let opts = SearchOptions {
        sort: SearchSort::Size,
        limit: 2,
        chunk: 16,
        mode: SizeMode::Allocated,
        ..SearchOptions::default()
    };
    let out = idx
        .search(&q, &opts, &CancelToken::new(), &|b| {
            batches.fetch_add(1, Ordering::Relaxed);
            streamed.lock().unwrap().extend_from_slice(b);
        })
        .unwrap();
    assert_eq!(out.hits.len(), 2);
    assert!(out.hits[0].size >= out.hits[1].size);
    assert_eq!(idx.name_lossy(out.hits[0].id), "IMG_0001.JPG");
    assert_eq!(streamed.lock().unwrap().len() as u64, out.matched);
    assert!(batches.load(Ordering::Relaxed) >= 1);
}

#[test]
fn cancelled_search_stops() {
    let idx = tree();
    let q = Query::parse("e", now()).unwrap();
    let token = CancelToken::new();
    token.cancel();
    let out = idx
        .search(&q, &SearchOptions::default(), &token, &|_| {
            panic!("no batches after cancel")
        })
        .unwrap();
    assert!(out.cancelled);
    assert!(out.hits.is_empty());
}

#[test]
fn live_entries_are_searchable() {
    let mut idx = tree();
    idx.upsert(file(r(500, 1), r(31, 1), "fresh-download.iso", 7))
        .unwrap();
    idx.upsert(file(r(42, 1), r(31, 1), "renamed-report", 50))
        .unwrap();
    idx.remove(r(43, 1)).unwrap();
    assert_eq!(run(&idx, "download"), ["fresh-download.iso"]);
    assert_eq!(run(&idx, "report"), ["Report-2024.pdf", "renamed-report"]);
    let attr = run(&idx, "attr:hidden");
    assert!(attr.is_empty());
    let q = Query::parse("attr:orphan", FileTime(0)).unwrap();
    assert!(q.filter.flags_all.contains(EntryFlags::ORPHAN));
}
