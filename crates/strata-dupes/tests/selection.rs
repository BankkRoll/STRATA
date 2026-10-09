//! Guardrail property tests: no sequence of selection operations can mark
//! every copy of a group, and no queue it produces deletes every copy.

use std::collections::BTreeMap;

use proptest::prelude::*;
use strata_core::{FileRef, FileTime, Safety};
use strata_dupes::*;

fn report(sizes: &[usize]) -> DuplicateReport {
    let groups = sizes
        .iter()
        .enumerate()
        .map(|(g, &n)| DuplicateGroup {
            id: g as u64,
            size: 1 << 20,
            hash: [g as u8; 32],
            files: (0..n)
                .map(|i| DupFile {
                    volume_serial: 1,
                    file_ref: FileRef::from_parts((g * 100 + i) as u64 + 64, 1),
                    path: format!(r"D:\g{g}\f{i}.bin").into(),
                    size: 1 << 20,
                    mtime: FileTime(1),
                    links: 1,
                })
                .collect(),
            keep: KeepSuggestion {
                index: n / 2,
                reason: KeepReason::FirstByPath,
            },
        })
        .collect();
    DuplicateReport {
        groups,
        ..DuplicateReport::default()
    }
}

#[derive(Debug, Clone)]
enum Op {
    Mark(u64, usize),
    Unmark(u64, usize),
    Keep(u64, usize),
    Apply(u64, Option<usize>, Vec<usize>),
    AllButKeeper,
}

fn op(groups: usize) -> impl Strategy<Value = Op> {
    let g = 0..groups as u64 + 1; // includes an unknown group
    let i = 0usize..9; // includes out-of-range indices
    prop_oneof![
        (g.clone(), i.clone()).prop_map(|(g, i)| Op::Mark(g, i)),
        (g.clone(), i.clone()).prop_map(|(g, i)| Op::Unmark(g, i)),
        (g.clone(), i.clone()).prop_map(|(g, i)| Op::Keep(g, i)),
        (
            g,
            proptest::option::of(i.clone()),
            proptest::collection::vec(i, 0..10)
        )
            .prop_map(|(g, k, m)| Op::Apply(g, k, m)),
        Just(Op::AllButKeeper),
    ]
}

fn check(sel: &Selection, r: &DuplicateReport) {
    for g in &r.groups {
        let s = sel.group(g.id).unwrap();
        assert!(s.marked_count() < g.files.len());
        assert!(!s.is_marked(s.keeper()));
    }
    let items = sel.to_queue_items(r, |_| Safety::Probably).unwrap();
    let mut per_group: BTreeMap<u64, usize> = BTreeMap::new();
    for it in &items {
        *per_group.entry(parse_queue_item_id(it.id).0).or_default() += 1;
    }
    for (g, n) in per_group {
        assert!(n < r.group(g).unwrap().files.len());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn no_operation_sequence_selects_every_copy(
        sizes in proptest::collection::vec(2usize..7, 1..5),
        ops in proptest::collection::vec(op(4), 0..60),
    ) {
        let r = report(&sizes);
        let mut sel = Selection::new(&r);
        check(&sel, &r);
        for o in ops {
            let res = match o {
                Op::Mark(g, i) => sel.mark(g, i),
                Op::Unmark(g, i) => sel.unmark(g, i),
                Op::Keep(g, i) => sel.set_keeper(g, i),
                Op::Apply(g, keep, marked) => sel.apply(&SelectionRequest { group: g, keep, marked }),
                Op::AllButKeeper => {
                    sel = Selection::all_but_suggested(&r);
                    Ok(())
                }
            };
            let _ = res;
            check(&sel, &r);
        }
    }

    #[test]
    fn requests_marking_every_copy_are_refused(n in 2usize..8, keep in proptest::option::of(0usize..8)) {
        let r = report(&[n]);
        let mut sel = Selection::all_but_suggested(&r);
        let before = sel.clone();
        let req = SelectionRequest { group: 0, keep, marked: (0..n).collect() };
        prop_assert!(sel.apply(&req).is_err());
        prop_assert_eq!(&sel, &before);
    }
}

#[test]
fn marking_the_keeper_moves_the_keeper_role() {
    let r = report(&[3]);
    let mut sel = Selection::new(&r);
    let k = sel.group(0).unwrap().keeper();
    sel.mark(0, k).unwrap();
    let s = sel.group(0).unwrap();
    assert_ne!(s.keeper(), k);
    assert!(s.is_marked(k));
    let others: Vec<usize> = (0..3).filter(|&i| i != k && i != s.keeper()).collect();
    sel.mark(0, others[0]).unwrap();
    let last = sel.group(0).unwrap().keeper();
    assert_eq!(
        sel.mark(0, last),
        Err(SelectionError::WouldDeleteAllCopies { group: 0 })
    );
    assert_eq!(sel.group(0).unwrap().marked_count(), 2);
}

#[test]
fn a_selection_for_another_report_is_refused() {
    let r = report(&[3]);
    let sel = Selection::all_but_suggested(&r);
    let other = report(&[2]);
    assert_eq!(
        sel.to_queue_items(&other, |_| Safety::Probably),
        Err(SelectionError::ReportMismatch)
    );
}

#[test]
fn queue_items_carry_expectations() {
    let r = report(&[3, 2]);
    let sel = Selection::all_but_suggested(&r);
    let items = sel.to_queue_items(&r, |_| Safety::Careful).unwrap();
    assert_eq!(items.len(), 3);
    for it in items {
        let (g, i) = parse_queue_item_id(it.id);
        let f = &r.group(g).unwrap().files[i];
        assert_eq!(it.path, f.path);
        assert_eq!(it.expected.file_ref, f.file_ref);
        assert_eq!(it.expected.size, f.size);
        assert_eq!(it.expected.modified, f.mtime);
        assert!(!it.expected.is_dir);
        assert_eq!(it.safety, Safety::Careful);
    }
}

#[test]
fn hardlink_plan_refuses_other_volumes_and_empty_selections() {
    let mut r = report(&[3]);
    let mut sel = Selection::new(&r);
    assert_eq!(
        hardlink::plan(&r, &sel, 0),
        Err(hardlink::HardlinkRefusal::NothingMarked)
    );
    sel = Selection::all_but_suggested(&r);
    let a = hardlink::plan(&r, &sel, 0).unwrap();
    assert_eq!(a.replace.len(), 2);
    assert_eq!(a.bytes_saved, 2 << 20);
    let text = strata_clean::consent::ConsentAction::describe(&a);
    assert!(text.contains("editing any one of them changes all"));
    r.groups[0].files[0].volume_serial = 2;
    assert!(matches!(
        hardlink::plan(&r, &sel, 0),
        Err(hardlink::HardlinkRefusal::DifferentVolume { .. })
    ));
    r.groups[0].files[0].volume_serial = 1;
    r.groups[0].files[1].links = hardlink::MAX_LINKS;
    assert!(matches!(
        hardlink::plan(&r, &sel, 0),
        Err(hardlink::HardlinkRefusal::TooManyLinks { .. })
    ));
}
