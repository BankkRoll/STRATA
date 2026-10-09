//! Subtree rescans for volumes without a journal: reconciling a re-walk
//! into the index must give the same result as a fresh build.

mod support;

use std::collections::HashSet;

use strata_core::{ScanRecord, WideName};
use strata_live::{reconcile_subtree, resolve_relative};
use support::{Model, ROOT_REC, Versions, canonical, diff};

/// Root → d → {files, sub → {files, deep → file}}, plus a sibling tree.
fn model() -> (Model, u64, u64) {
    let mut m = Model::new(Versions::V2);
    let d = m.create_in(ROOT_REC, true, 0, false);
    let sub = m.create_in(d, true, 0, false);
    let deep = m.create_in(sub, true, 0, false);
    m.create_in(deep, false, 4242, false);
    for i in 0..5 {
        m.create_in(d, false, 100 * i, false);
        m.create_in(sub, false, 1000 * i, false);
    }
    let other = m.create_in(ROOT_REC, true, 0, false);
    m.create_in(other, false, 7, false);
    (m, d, sub)
}

fn under(m: &Model, dir: u64, deep: bool) -> Vec<ScanRecord> {
    let mut keep: HashSet<u64> = HashSet::from([dir]);
    loop {
        let before = keep.len();
        for (&rec, n) in &m.nodes {
            let p = n.links[0].0;
            if rec != ROOT_REC && keep.contains(&p) && (deep || p == dir) {
                keep.insert(rec);
            }
        }
        if keep.len() == before {
            break;
        }
    }
    m.records()
        .into_iter()
        .filter(|r| keep.contains(&r.id.record()))
        .collect()
}

fn assert_same(index: &strata_index::Index, m: &Model) {
    index.check_invariants().expect("invariants");
    if let Some(d) = diff(&canonical(index), &canonical(&m.build_index())) {
        panic!("{d}");
    }
}

#[test]
fn deep_rescan_reconciles_a_subtree() {
    let (mut m, d, sub) = model();
    let mut index = m.build_index();
    let files: Vec<u64> = m
        .nodes
        .iter()
        .filter(|(_, n)| !n.dir && n.links[0].0 == sub)
        .map(|(r, _)| *r)
        .collect();
    m.delete_rec(files[0]);
    m.write_rec(files[1], 999_999, false, false);
    let nd = m.create_in(sub, true, 0, false);
    m.create_in(nd, false, 31_337, false);
    m.rename_link(files[2], 0, d, false);

    let scope = index.lookup(m.file_ref(d)).expect("d");
    let cs = reconcile_subtree(&mut index, scope, under(&m, d, true), true).expect("apply");
    assert!(!cs.is_empty());
    assert_same(&index, &m);
}

#[test]
fn shallow_rescan_removes_vanished_folders_with_their_subtrees() {
    let (mut m, d, sub) = model();
    let mut index = m.build_index();
    m.delete_rec(sub);
    m.create_in(d, false, 5, false);
    let first = m
        .nodes
        .iter()
        .find(|(_, n)| !n.dir && n.links[0].0 == d)
        .map(|(r, _)| *r)
        .expect("file");
    m.write_rec(first, 1, false, false);

    let scope = index.lookup(m.file_ref(d)).expect("d");
    reconcile_subtree(&mut index, scope, under(&m, d, false), false).expect("apply");
    assert_same(&index, &m);
}

#[test]
fn resolves_relative_paths() {
    let (m, d, sub) = model();
    let index = m.build_index();
    let name = |rec: u64| WideName::from_str_lossless(&m.nodes[&rec].links[0].1);
    let got = resolve_relative(&index, index.root(), &[name(d), name(sub)]);
    assert_eq!(got, index.lookup(m.file_ref(sub)));
    assert_eq!(
        resolve_relative(&index, index.root(), &[WideName::from_str_lossless("nope")]),
        None
    );
    assert_eq!(
        resolve_relative(&index, index.root(), &[]),
        Some(index.root())
    );
}
