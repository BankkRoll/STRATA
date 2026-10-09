//! Merging consecutive [`ChangeSet`]s into one.
//!
//! A tick applies its updates in several short batches (so the index lock
//! is never held for long) and reports one merged change set. Within one
//! `ChangeSet` the index reports an id in both `removed` and `created` when a
//! slot was freed and reused; the merge keeps that convention, so consumers
//! process `removed` before `created`.

use std::collections::HashMap;

use strata_index::{ChangeSet, DirAggregate, EntryId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Created,
    Updated,
    Removed,
    /// Existed before, removed, slot reused by a new entry.
    Replaced,
}

/// Accumulates change sets in application order.
#[derive(Debug, Default)]
pub struct ChangeMerger {
    states: HashMap<EntryId, State>,
    aggregates: HashMap<EntryId, DirAggregate>,
}

impl ChangeMerger {
    /// Adds the next change set.
    pub fn push(&mut self, cs: ChangeSet) {
        for id in cs.removed {
            match self.states.get(&id) {
                Some(State::Created) => {
                    self.states.remove(&id);
                }
                _ => {
                    self.states.insert(id, State::Removed);
                }
            }
            self.aggregates.remove(&id);
        }
        for id in cs.created {
            let next = match self.states.get(&id) {
                Some(State::Removed | State::Replaced) => State::Replaced,
                _ => State::Created,
            };
            self.states.insert(id, next);
        }
        for id in cs.updated {
            self.states.entry(id).or_insert(State::Updated);
        }
        for (id, agg) in cs.aggregates {
            self.aggregates.insert(id, agg);
        }
    }

    /// Whether nothing was merged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty() && self.aggregates.is_empty()
    }

    /// The merged change set; ids are sorted and unique per list.
    #[must_use]
    pub fn finish(self) -> ChangeSet {
        let mut cs = ChangeSet::default();
        let states = &self.states;
        cs.aggregates = self
            .aggregates
            .into_iter()
            .filter(|(id, _)| states.get(id) != Some(&State::Removed))
            .collect();
        for (id, s) in self.states {
            match s {
                State::Created => cs.created.push(id),
                State::Updated => cs.updated.push(id),
                State::Removed => cs.removed.push(id),
                State::Replaced => {
                    cs.created.push(id);
                    cs.removed.push(id);
                }
            }
        }
        cs.created.sort_unstable();
        cs.updated.sort_unstable();
        cs.removed.sort_unstable();
        cs.aggregates.sort_unstable_by_key(|(id, _)| *id);
        cs
    }
}

/// Merges change sets given in application order.
///
/// # Example
///
/// ```
/// use strata_index::{ChangeSet, EntryId};
/// use strata_live::merge_change_sets;
///
/// let a = ChangeSet { created: vec![EntryId(9)], ..ChangeSet::default() };
/// let b = ChangeSet { removed: vec![EntryId(9)], ..ChangeSet::default() };
/// assert!(merge_change_sets([a, b]).is_empty());
/// ```
#[must_use]
pub fn merge_change_sets(sets: impl IntoIterator<Item = ChangeSet>) -> ChangeSet {
    let mut m = ChangeMerger::default();
    for cs in sets {
        m.push(cs);
    }
    m.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(created: &[u32], updated: &[u32], removed: &[u32], aggs: &[(u32, u64)]) -> ChangeSet {
        ChangeSet {
            created: created.iter().copied().map(EntryId).collect(),
            updated: updated.iter().copied().map(EntryId).collect(),
            removed: removed.iter().copied().map(EntryId).collect(),
            aggregates: aggs
                .iter()
                .map(|&(id, a)| {
                    (
                        EntryId(id),
                        DirAggregate {
                            allocated: a,
                            ..DirAggregate::default()
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn transitions() {
        let out = merge_change_sets([
            cs(&[1, 2], &[3, 4], &[5], &[(1, 10), (3, 1)]),
            cs(&[5], &[1, 3], &[2, 4], &[(3, 2)]),
        ]);
        // 1 created then updated: created. 2 created then removed: gone.
        // 3 updated twice. 4 updated then removed. 5 removed then reused.
        assert_eq!(out.created, vec![EntryId(1), EntryId(5)]);
        assert_eq!(out.updated, vec![EntryId(3)]);
        assert_eq!(out.removed, vec![EntryId(4), EntryId(5)]);
        let aggs: Vec<(u32, u64)> = out
            .aggregates
            .iter()
            .map(|(i, a)| (i.0, a.allocated))
            .collect();
        assert_eq!(aggs, vec![(1, 10), (3, 2)]);
    }

    #[test]
    fn replaced_then_removed_is_removed() {
        let out = merge_change_sets([cs(&[7], &[], &[7], &[]), cs(&[], &[], &[7], &[(7, 1)])]);
        assert_eq!(out.removed, vec![EntryId(7)]);
        assert!(out.created.is_empty());
        assert!(out.aggregates.is_empty());
    }
}
