//! End to end on a real folder this test creates: walker → index (with the
//! streaming preview) → classification → layout frame, row page, entry info,
//! rescan id remap, and search streaming with cancellation.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use strata_app_lib::classify::{Engine, safety_code};
use strata_app_lib::layout_pipe::{LayoutRequest, LayoutStream, Style, TransformDto, View};
use strata_app_lib::model::{ScannerUsed, ViewFilters};
use strata_app_lib::rows::{RowQuery, Sort, SortKey, row_page};
use strata_app_lib::scan::{
    DataSlot, Ingest, ScanObserver, ScanProgress, ScanTarget, ShadowBytes, walk,
};
use strata_app_lib::search::{SearchBatch, SearchQuery, SearchStream, Target};
use strata_app_lib::{detail, ids};
use strata_core::known::KnownFolders;
use strata_core::{Category, SizeMode};
use strata_index::EntryId;

/// A directory under the system temp folder, removed on drop. Only this
/// directory is ever written or deleted.
struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("strata-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn file(&self, rel: &str, bytes: usize) {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![b'x'; bytes]).unwrap();
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Counts {
    progress: AtomicUsize,
    previews: AtomicUsize,
}

impl ScanObserver for Counts {
    fn progress(&self, _: &ScanProgress) {
        self.progress.fetch_add(1, Ordering::Relaxed);
    }
    fn preview(&self) {
        self.previews.fetch_add(1, Ordering::Relaxed);
    }
}

fn scan(engine: &Arc<Engine>, root: &Path, slot: &Arc<DataSlot>, obs: &Counts) {
    let display = root.to_string_lossy().into_owned();
    let target = ScanTarget {
        root: root.to_path_buf(),
        root_display: display,
        volume: None,
    };
    let mut ingest = Ingest::new(
        engine.clone(),
        target,
        slot.clone(),
        ScannerUsed::Walker,
        true,
    );
    let stats = walk(
        &mut ingest,
        strata_walk::WalkOptions::default(),
        &strata_walk::CancelToken::new(),
        obs,
    )
    .unwrap();
    ingest
        .finish(stats.partial, ShadowBytes::default())
        .unwrap();
}

fn find(data: &strata_app_lib::model::VolumeData, name: &str) -> EntryId {
    let ix = &data.index;
    (0..ix.slot_count() as u32)
        .map(EntryId)
        .find(|&e| ix.is_live(e) && ix.name_lossy(e) == name)
        .unwrap_or_else(|| panic!("{name} not indexed"))
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

#[test]
fn walker_to_frame_rows_search() {
    let tree = TempTree::new("pipeline");
    tree.file("proj/package.json", 20);
    tree.file("proj/node_modules/left-pad/index.js", 10_000);
    tree.file("docs/big.bin", 100_000);
    tree.file("docs/notes.txt", 10);
    tree.file("debug.tmp", 50);

    let engine = Arc::new(Engine::new(&KnownFolders::default(), None).unwrap());
    let slot: Arc<DataSlot> = Arc::new(RwLock::new(None));
    let obs = Counts::default();
    scan(&engine, &tree.0, &slot, &obs);
    assert!(
        obs.previews.load(Ordering::Relaxed) >= 1,
        "a preview was published"
    );

    let guard = slot.read().unwrap();
    let data = guard.as_ref().unwrap();
    assert!(!data.preview);
    assert_eq!(data.generation, 1, "preview was generation 0");
    let ix = &data.index;
    let root = ix.root();
    assert_eq!(
        ix.size(root, SizeMode::Logical),
        20 + 10_000 + 100_000 + 10 + 50
    );

    // Classification: node_modules next to package.json is safe dev data.
    let nm = find(data, "node_modules");
    let c = data.class(nm).unwrap();
    assert_eq!(
        engine.classifier.rule(c.rule.unwrap()).id,
        "dev.node_modules"
    );
    assert_eq!(c.category, Category::DevBuild);
    assert_eq!(safety_code(data.class_bits(nm)), 1);
    let key = data.color_key(nm, strata_app_lib::model::now_epoch2000());
    assert_eq!(key & 0xF, Category::DevBuild as u32);
    assert_eq!((key >> 4) & 7, 1);
    assert!(((key >> 7) & 0x1F) >= 1, "fresh files have an age bucket");
    // The file inherits the folder's classification and its app label.
    let js = find(data, "index.js");
    assert_eq!(data.class(js).unwrap().category, Category::DevBuild);
    assert_ne!(ix.owner_app(nm), 0);
    let info = detail::entry_info(data, Some(&engine), js);
    assert_eq!(info.app, engine.apps.name(ix.owner_app(nm)));
    assert_eq!(info.safety, Some("safe"));
    let tmp = find(data, "debug.tmp");
    assert_eq!(
        engine
            .classifier
            .rule(data.class(tmp).unwrap().rule.unwrap())
            .id,
        "generic.tmp_files"
    );

    // Layout frame for every view.
    let frames = Arc::new(Mutex::new(Vec::new()));
    let sink = frames.clone();
    let stream = LayoutStream::new(Box::new(move |b| {
        sink.lock().unwrap().push(b);
        Ok(())
    }));
    for (seq, view) in [
        View::Treemap,
        View::Icicle,
        View::Flame,
        View::Sunburst,
        View::Bubbles,
        View::Mindmap,
    ]
    .into_iter()
    .enumerate()
    {
        let req = LayoutRequest {
            volume_id: "test".into(),
            root: data.root_wire(),
            view,
            width: 800.0,
            height: 600.0,
            dpr: 1.0,
            transform: TransformDto {
                scale: 1.0,
                tx: 0.0,
                ty: 0.0,
            },
            size_mode: SizeMode::Logical,
            style: Style::Cushion,
            filters: ViewFilters::default(),
            animate: true,
        };
        let seq = seq as u32 + 1;
        stream.announce(seq);
        assert!(stream.serve(data, &req, seq).unwrap());
    }
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 6);
    let f = &frames[0];
    assert_eq!(u32_at(f, 0), u32::from_le_bytes(*b"STLF"));
    assert_eq!(u32_at(f, 8), 1);
    assert_eq!(u32_at(f, 12), data.root_wire());
    assert_eq!(
        &f[112..120],
        &ix.size(root, SizeMode::Logical).to_le_bytes()
    );
    let (off, len) = (u32_at(f, 72) as usize, u32_at(f, 76) as usize);
    assert!(len >= 32 && len % 32 == 0);
    for r in (off..off + len).step_by(32) {
        let id = u32_at(f, r + 16);
        assert!(data.resolve(id).is_some(), "record ids are live wire ids");
    }
    assert_ne!(u32_at(f, 28) & 1, 0, "cushion section present");
    assert_eq!(
        u16::from_le_bytes([frames[3][6], frames[3][7]]),
        3,
        "sunburst"
    );

    // Stale requests are dropped.
    stream.announce(100);
    let req = LayoutRequest {
        volume_id: "test".into(),
        root: data.root_wire(),
        view: View::Treemap,
        width: 10.0,
        height: 10.0,
        dpr: 1.0,
        transform: TransformDto {
            scale: 1.0,
            tx: 0.0,
            ty: 0.0,
        },
        size_mode: SizeMode::Allocated,
        style: Style::Flat,
        filters: ViewFilters::default(),
        animate: false,
    };
    drop(frames);
    assert!(!stream.serve(data, &req, 99).unwrap());

    // Row page, largest first.
    let page = row_page(
        data,
        &RowQuery {
            volume_id: "test".into(),
            parent: data.root_wire(),
            sort: Sort {
                key: SortKey::Size,
                desc: true,
            },
            size_mode: SizeMode::Logical,
            offset: 0,
            limit: 200,
            filters: ViewFilters::default(),
        },
    )
    .unwrap();
    assert_eq!(u32_at(&page, 0), u32::from_le_bytes(*b"STRP"));
    assert_eq!(u32_at(&page, 20), 3, "docs, proj, debug.tmp");
    let first = u32_at(&page, 32);
    assert_eq!(data.resolve(first), Some(find(data, "docs")));
    drop(guard);

    // A rescan bumps the generation; old ids resolve through the remap.
    let old_docs = {
        let g = slot.read().unwrap();
        let d = g.as_ref().unwrap();
        d.wire(find(d, "docs"))
    };
    tree.file("docs/more.bin", 4096);
    scan(&engine, &tree.0, &slot, &Counts::default());
    {
        let g = slot.read().unwrap();
        let d = g.as_ref().unwrap();
        assert_eq!(d.generation, 2);
        assert_eq!(ids::decode(old_docs).0, 1);
        assert_eq!(d.resolve(old_docs), Some(find(d, "docs")));
        assert_eq!(d.index.size(find(d, "docs"), SizeMode::Logical), 104_106);
    }

    // Search: a query issued while the index is write-locked and then
    // superseded sends nothing; the latest query completes.
    let batches = Arc::new(Mutex::new(Vec::<SearchBatch>::new()));
    let sink = batches.clone();
    let search = SearchStream::new(Arc::new(move |b| sink.lock().unwrap().push(b)));
    let targets = vec![Target {
        volume_id: "test".into(),
        slot: slot.clone(),
    }];
    let q = |text: &str| SearchQuery {
        text: text.into(),
        regex: false,
        case_sensitive: false,
        volume_id: None,
    };
    let w = slot.write().unwrap();
    let first = search.query(1, q("big"), targets.clone(), Some(engine.clone()));
    let second = search.query(2, q("notes"), targets.clone(), Some(engine.clone()));
    drop(w);
    first.join().unwrap();
    second.join().unwrap();
    let got = batches.lock().unwrap();
    assert!(
        got.iter().all(|b| b.seq == 2),
        "cancelled query sent nothing"
    );
    let last = got.last().unwrap();
    assert!(last.done);
    assert_eq!(last.total, Some(1));
    assert_eq!(last.results[0].name, "notes.txt");
    assert!(last.results[0].parent_path.ends_with("docs"));
    drop(got);

    // Filters: `safe:yes` uses the classifier's tiers.
    batches.lock().unwrap().clear();
    search
        .query(3, q("safe:yes file:"), targets, Some(engine.clone()))
        .join()
        .unwrap();
    let got = batches.lock().unwrap();
    let names: Vec<&str> = got
        .iter()
        .flat_map(|b| b.results.iter().map(|r| r.name.as_str()))
        .collect();
    assert!(names.contains(&"index.js"));
    assert!(!names.contains(&"notes.txt"));
}
