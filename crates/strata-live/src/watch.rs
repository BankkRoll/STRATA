//! Volumes without a usable USN journal (FAT, exFAT, network shares, ReFS
//! with 128-bit ids): an optional `ReadDirectoryChangesW`-style watcher on
//! the subtree the user is viewing, plus manual rescans.
//!
//! The app implements [`SubtreeWatcher`] and the walker. This module turns
//! watcher notifications into a minimal [`RescanTarget`] list
//! ([`RescanPlanner`]) and folds a rescan's records back into the index
//! ([`reconcile_subtree`]). A manual "Rescan" of any folder is one deep
//! target.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use strata_core::{FileRef, ScanRecord, WideName};
use strata_index::{ChangeSet, EntryId, Index, IndexError, Update};

use crate::source::SourceError;

/// Kind of one watcher notification (`FILE_ACTION_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    /// A name appeared.
    Added,
    /// A name disappeared.
    Removed,
    /// Size, times or attributes changed.
    Modified,
    /// Old name of a rename.
    RenamedFrom,
    /// New name of a rename.
    RenamedTo,
}

/// One notification; `path` is relative to the watched folder, one
/// component per element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchChange {
    /// What happened.
    pub kind: WatchKind,
    /// Relative path components.
    pub path: Vec<WideName>,
}

/// What one wait on the watcher produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchBatch {
    /// Notifications in order.
    Changes(Vec<WatchChange>),
    /// The notification buffer overflowed; anything may have changed.
    Overflow,
}

/// Watches one folder tree (implemented by the app).
pub trait SubtreeWatcher {
    /// Blocks until notifications arrive or `wait` elapses (`None`: no
    /// limit). Returning an empty batch is allowed.
    ///
    /// # Errors
    ///
    /// Any [`SourceError`]; [`SourceError::VolumeGone`] when the folder or
    /// volume disappeared.
    fn next(&mut self, wait: Option<Duration>) -> Result<WatchBatch, SourceError>;
}

/// A folder to re-walk, relative to the watched folder.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RescanTarget {
    /// Relative path components; empty is the watched folder itself.
    pub path: Vec<WideName>,
    /// Walk the whole subtree (`true`) or only the folder's direct entries.
    pub deep: bool,
}

/// Accumulates notifications into the smallest set of rescans.
///
/// # Example
///
/// ```
/// use strata_core::WideName;
/// use strata_live::{RescanPlanner, WatchBatch, WatchChange, WatchKind};
///
/// let p = |s: &str| s.split('\\').map(WideName::from_str_lossless).collect::<Vec<_>>();
/// let mut plan = RescanPlanner::default();
/// plan.push(WatchBatch::Changes(vec![
///     WatchChange { kind: WatchKind::Modified, path: p("a\\b.txt") },
///     WatchChange { kind: WatchKind::Added, path: p("a\\new") },
/// ]));
/// let targets = plan.take();
/// assert_eq!(targets.len(), 2); // "a" shallow, "a\new" deep
/// ```
#[derive(Debug, Default)]
pub struct RescanPlanner {
    /// Path → deep.
    targets: BTreeMap<Vec<WideName>, bool>,
}

impl RescanPlanner {
    /// Adds one batch.
    pub fn push(&mut self, batch: WatchBatch) {
        match batch {
            WatchBatch::Overflow => self.add(Vec::new(), true),
            WatchBatch::Changes(changes) => {
                for c in changes {
                    let parent = c.path[..c.path.len().saturating_sub(1)].to_vec();
                    match c.kind {
                        WatchKind::Modified | WatchKind::Removed | WatchKind::RenamedFrom => {
                            self.add(parent, false);
                        }
                        WatchKind::Added | WatchKind::RenamedTo => {
                            self.add(parent, false);
                            if !c.path.is_empty() {
                                self.add(c.path, true);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Requests a manual rescan of `path` (deep).
    pub fn rescan(&mut self, path: Vec<WideName>) {
        self.add(path, true);
    }

    fn add(&mut self, path: Vec<WideName>, deep: bool) {
        let e = self.targets.entry(path).or_insert(deep);
        *e |= deep;
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Takes the queued targets, dropping any covered by a deep ancestor.
    pub fn take(&mut self) -> Vec<RescanTarget> {
        let all = std::mem::take(&mut self.targets);
        let deep: Vec<&Vec<WideName>> = all.iter().filter(|(_, d)| **d).map(|(p, _)| p).collect();
        all.iter()
            .filter(|(path, _)| {
                !deep
                    .iter()
                    .any(|d| path.len() > d.len() && path.starts_with(d))
            })
            .map(|(path, deep)| RescanTarget {
                path: path.clone(),
                deep: *deep,
            })
            .collect()
    }
}

/// Resolves a relative path below `base` by exact name match.
#[must_use]
pub fn resolve_relative(index: &Index, base: EntryId, path: &[WideName]) -> Option<EntryId> {
    let mut cur = base;
    for comp in path {
        cur = index.children(cur).find(|&c| index.name(c) == *comp)?;
    }
    Some(cur)
}

/// Folds the result of re-walking `scope` into the index.
///
/// `fresh` holds the records the walk found below `scope`: its direct
/// entries when `deep` is false, its whole subtree when true. It may also
/// hold `scope`'s own record, which is then refreshed too. Entries that are no longer there are removed together with their
/// subtrees; everything found is upserted.
///
/// # Errors
///
/// The first [`IndexError`] from [`Index::apply`].
pub fn reconcile_subtree(
    index: &mut Index,
    scope: EntryId,
    fresh: Vec<ScanRecord>,
    deep: bool,
) -> Result<ChangeSet, IndexError> {
    let keep: HashSet<FileRef> = fresh.iter().map(|r| r.id).collect();
    let mut gone: Vec<FileRef> = Vec::new();
    let existing: Vec<EntryId> = if deep {
        let mut v = Vec::new();
        index.for_each_in_subtree(scope, |e| {
            if e != scope {
                v.push(e);
            }
        });
        v
    } else {
        index.children(scope).collect()
    };
    let mut seen: HashSet<FileRef> = HashSet::new();
    for e in existing {
        let Some(r) = index.file_ref(e) else { continue };
        if keep.contains(&r) || !seen.insert(r) {
            continue;
        }
        gone.push(r);
        if !deep {
            // A vanished folder takes its subtree with it.
            index.for_each_in_subtree(e, |d| {
                if d != e
                    && let Some(dr) = index.file_ref(d)
                    && !keep.contains(&dr)
                    && seen.insert(dr)
                {
                    gone.push(dr);
                }
            });
        }
    }
    let updates = gone
        .into_iter()
        .map(Update::Remove)
        .chain(fresh.into_iter().map(Update::Upsert));
    index.set_now(crate::source::wall_clock_now());
    index.apply(updates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Vec<WideName> {
        if s.is_empty() {
            return Vec::new();
        }
        s.split('\\').map(WideName::from_str_lossless).collect()
    }

    fn t(s: &str, deep: bool) -> RescanTarget {
        RescanTarget { path: p(s), deep }
    }

    fn change(kind: WatchKind, s: &str) -> WatchChange {
        WatchChange { kind, path: p(s) }
    }

    #[test]
    fn plans_minimal_targets() {
        let mut plan = RescanPlanner::default();
        plan.push(WatchBatch::Changes(vec![
            change(WatchKind::Modified, "a\\x.txt"),
            change(WatchKind::Modified, "a\\y.txt"),
            change(WatchKind::Removed, "b\\gone"),
            change(WatchKind::RenamedFrom, "c\\old"),
            change(WatchKind::RenamedTo, "d\\new"),
            change(WatchKind::Added, "d\\new\\inner.txt"),
        ]));
        assert_eq!(
            plan.take(),
            vec![
                t("a", false),
                t("b", false),
                t("c", false),
                t("d", false),
                t("d\\new", true),
            ]
        );
        assert!(plan.is_empty());
    }

    #[test]
    fn overflow_covers_everything() {
        let mut plan = RescanPlanner::default();
        plan.push(WatchBatch::Changes(vec![change(
            WatchKind::Modified,
            "a\\x",
        )]));
        plan.push(WatchBatch::Overflow);
        assert_eq!(plan.take(), vec![t("", true)]);
    }

    #[test]
    fn manual_rescan_upgrades_a_shallow_target() {
        let mut plan = RescanPlanner::default();
        plan.push(WatchBatch::Changes(vec![change(
            WatchKind::Modified,
            "a\\x",
        )]));
        plan.rescan(p("a"));
        assert_eq!(plan.take(), vec![t("a", true)]);
    }
}
