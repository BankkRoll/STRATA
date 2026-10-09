//! The deletion selection, with the "never every copy" guardrail enforced
//! by construction.
//!
//! Each [`GroupSelection`] has a **keeper** that can never be marked. Every
//! mutator preserves that invariant (marking the keeper moves the keeper
//! role to an unmarked copy, or fails when there is none), the fields are
//! private, and there is no `Deserialize`: the UI sends a
//! [`SelectionRequest`], which is validated as a whole before it replaces
//! anything. [`Selection::to_queue_items`] checks the invariant once more
//! against the report before producing `strata-clean` queue items.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use strata_clean::flow::QueueItem;
use strata_core::Safety;

use crate::report::{DupFile, DuplicateReport};

/// Bits of a queue item id that hold the file index.
const INDEX_BITS: u32 = 24;

/// Largest group the selection accepts (file indices must fit the id).
pub const MAX_GROUP_FILES: usize = 1 << INDEX_BITS;

/// Why a selection change was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionError {
    /// The change would select every copy in the group.
    #[error("at least one copy in group {group} must be kept")]
    WouldDeleteAllCopies {
        /// Group id.
        group: u64,
    },
    /// No such group in the report.
    #[error("unknown duplicate group {group}")]
    UnknownGroup {
        /// Group id.
        group: u64,
    },
    /// File index out of range.
    #[error("group {group} has no file {index}")]
    NoSuchFile {
        /// Group id.
        group: u64,
        /// File index.
        index: usize,
    },
    /// The selection was made for a different report.
    #[error("the selection does not match the current results; rescan")]
    ReportMismatch,
    /// Too many files in one group.
    #[error("group {group} is too large to select from")]
    GroupTooLarge {
        /// Group id.
        group: u64,
    },
}

/// Marked copies of one group plus the copy that is always kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupSelection {
    group: u64,
    keeper: usize,
    marked: Vec<bool>,
}

impl GroupSelection {
    /// Nothing marked; `keeper` is kept.
    ///
    /// # Errors
    ///
    /// [`SelectionError::NoSuchFile`] when `keeper >= files`, or
    /// [`SelectionError::GroupTooLarge`].
    pub fn new(group: u64, files: usize, keeper: usize) -> Result<Self, SelectionError> {
        if files > MAX_GROUP_FILES {
            return Err(SelectionError::GroupTooLarge { group });
        }
        if keeper >= files {
            return Err(SelectionError::NoSuchFile {
                group,
                index: keeper,
            });
        }
        Ok(Self {
            group,
            keeper,
            marked: vec![false; files],
        })
    }

    /// Group id.
    #[must_use]
    pub fn group(&self) -> u64 {
        self.group
    }

    /// The copy that is kept whatever else is marked.
    #[must_use]
    pub fn keeper(&self) -> usize {
        self.keeper
    }

    /// Whether copy `i` is marked for deletion.
    #[must_use]
    pub fn is_marked(&self, i: usize) -> bool {
        self.marked.get(i).copied().unwrap_or(false)
    }

    /// Marked copies, ascending.
    pub fn marked(&self) -> impl Iterator<Item = usize> + '_ {
        self.marked
            .iter()
            .enumerate()
            .filter_map(|(i, &m)| m.then_some(i))
    }

    /// Number of marked copies (always less than the group size).
    #[must_use]
    pub fn marked_count(&self) -> usize {
        self.marked.iter().filter(|&&m| m).count()
    }

    fn check(&self, i: usize) -> Result<(), SelectionError> {
        if i < self.marked.len() {
            Ok(())
        } else {
            Err(SelectionError::NoSuchFile {
                group: self.group,
                index: i,
            })
        }
    }

    /// Marks copy `i`. Marking the keeper hands the keeper role to the
    /// first unmarked copy; if every other copy is marked, it fails.
    ///
    /// # Errors
    ///
    /// [`SelectionError::WouldDeleteAllCopies`] or
    /// [`SelectionError::NoSuchFile`]. The selection is unchanged on error.
    pub fn mark(&mut self, i: usize) -> Result<(), SelectionError> {
        self.check(i)?;
        if i == self.keeper {
            let Some(next) = (0..self.marked.len()).find(|&j| j != i && !self.marked[j]) else {
                return Err(SelectionError::WouldDeleteAllCopies { group: self.group });
            };
            self.keeper = next;
        }
        self.marked[i] = true;
        Ok(())
    }

    /// Unmarks copy `i`.
    ///
    /// # Errors
    ///
    /// [`SelectionError::NoSuchFile`].
    pub fn unmark(&mut self, i: usize) -> Result<(), SelectionError> {
        self.check(i)?;
        self.marked[i] = false;
        Ok(())
    }

    /// Makes `i` the keeper (unmarking it).
    ///
    /// # Errors
    ///
    /// [`SelectionError::NoSuchFile`].
    pub fn set_keeper(&mut self, i: usize) -> Result<(), SelectionError> {
        self.check(i)?;
        self.marked[i] = false;
        self.keeper = i;
        Ok(())
    }

    /// Marks every copy except the keeper.
    pub fn mark_all_but_keeper(&mut self) {
        for (i, m) in self.marked.iter_mut().enumerate() {
            *m = i != self.keeper;
        }
    }

    /// Unmarks everything.
    pub fn clear(&mut self) {
        self.marked.fill(false);
    }

    fn holds_invariant(&self) -> bool {
        self.keeper < self.marked.len() && !self.marked[self.keeper]
    }
}

/// What the UI sends for one group: the copy to keep and the copies to
/// delete. Validated as a whole by [`Selection::apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRequest {
    /// Group id.
    pub group: u64,
    /// Copy to keep; `None` keeps the current keeper (or picks the first
    /// copy not in `marked`).
    pub keep: Option<usize>,
    /// Copies to delete.
    pub marked: Vec<usize>,
}

/// The selection across every group of one report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Selection {
    groups: BTreeMap<u64, GroupSelection>,
}

impl Selection {
    /// Nothing marked; each group keeps its suggested copy.
    #[must_use]
    pub fn new(report: &DuplicateReport) -> Self {
        let groups = report
            .groups
            .iter()
            .filter_map(|g| {
                GroupSelection::new(
                    g.id,
                    g.files.len(),
                    g.keep.index.min(g.files.len().saturating_sub(1)),
                )
                .ok()
                .map(|s| (g.id, s))
            })
            .collect();
        Self { groups }
    }

    /// Every copy marked except each group's suggested keeper.
    #[must_use]
    pub fn all_but_suggested(report: &DuplicateReport) -> Self {
        let mut s = Self::new(report);
        for g in s.groups.values_mut() {
            g.mark_all_but_keeper();
        }
        s
    }

    /// One group's selection.
    #[must_use]
    pub fn group(&self, id: u64) -> Option<&GroupSelection> {
        self.groups.get(&id)
    }

    fn group_mut(&mut self, id: u64) -> Result<&mut GroupSelection, SelectionError> {
        self.groups
            .get_mut(&id)
            .ok_or(SelectionError::UnknownGroup { group: id })
    }

    /// See [`GroupSelection::mark`].
    ///
    /// # Errors
    ///
    /// As [`GroupSelection::mark`], or an unknown group.
    pub fn mark(&mut self, group: u64, index: usize) -> Result<(), SelectionError> {
        self.group_mut(group)?.mark(index)
    }

    /// See [`GroupSelection::unmark`].
    ///
    /// # Errors
    ///
    /// As [`GroupSelection::unmark`], or an unknown group.
    pub fn unmark(&mut self, group: u64, index: usize) -> Result<(), SelectionError> {
        self.group_mut(group)?.unmark(index)
    }

    /// See [`GroupSelection::set_keeper`].
    ///
    /// # Errors
    ///
    /// As [`GroupSelection::set_keeper`], or an unknown group.
    pub fn set_keeper(&mut self, group: u64, index: usize) -> Result<(), SelectionError> {
        self.group_mut(group)?.set_keeper(index)
    }

    /// Replaces one group's selection with a UI request, all or nothing.
    ///
    /// # Errors
    ///
    /// [`SelectionError::WouldDeleteAllCopies`] when `marked` covers every
    /// copy or contains `keep`; index and group errors otherwise.
    pub fn apply(&mut self, req: &SelectionRequest) -> Result<(), SelectionError> {
        let current = self.group_mut(req.group)?;
        let n = current.marked.len();
        let mut marked = vec![false; n];
        for &i in &req.marked {
            *marked.get_mut(i).ok_or(SelectionError::NoSuchFile {
                group: req.group,
                index: i,
            })? = true;
        }
        let keeper = match req.keep {
            Some(k) if k >= n => {
                return Err(SelectionError::NoSuchFile {
                    group: req.group,
                    index: k,
                });
            }
            Some(k) => k,
            None if !marked[current.keeper] => current.keeper,
            None => marked
                .iter()
                .position(|&m| !m)
                .ok_or(SelectionError::WouldDeleteAllCopies { group: req.group })?,
        };
        if marked[keeper] {
            return Err(SelectionError::WouldDeleteAllCopies { group: req.group });
        }
        current.keeper = keeper;
        current.marked = marked;
        Ok(())
    }

    /// Bytes the marked copies occupy.
    #[must_use]
    pub fn marked_bytes(&self, report: &DuplicateReport) -> u64 {
        self.groups
            .values()
            .filter_map(|s| {
                report
                    .group(s.group)
                    .map(|g| g.size.saturating_mul(s.marked_count() as u64))
            })
            .sum()
    }

    /// Every marked copy as `(group id, file index, file)`.
    ///
    /// # Errors
    ///
    /// [`SelectionError::ReportMismatch`] when the report's groups differ
    /// from the ones this selection was made for.
    pub fn marked_files<'r>(
        &self,
        report: &'r DuplicateReport,
    ) -> Result<Vec<(u64, usize, &'r DupFile)>, SelectionError> {
        let mut out = Vec::new();
        for s in self.groups.values() {
            let g = report
                .group(s.group)
                .ok_or(SelectionError::ReportMismatch)?;
            if g.files.len() != s.marked.len() {
                return Err(SelectionError::ReportMismatch);
            }
            // Defense in depth: the invariant is maintained by every
            // mutator, but a delete must never rest on that alone.
            if !s.holds_invariant() || s.marked_count() >= g.files.len() {
                return Err(SelectionError::WouldDeleteAllCopies { group: s.group });
            }
            let keeper = &g.files[s.keeper];
            for i in s.marked() {
                let f = &g.files[i];
                if f.volume_serial == keeper.volume_serial && f.file_ref == keeper.file_ref {
                    // The same file twice would make "keep one" a lie.
                    return Err(SelectionError::WouldDeleteAllCopies { group: s.group });
                }
                out.push((s.group, i, f));
            }
        }
        Ok(out)
    }

    /// `strata-clean` queue items for every marked copy, carrying the
    /// expected file id, size and mtime for the cleaner's pre-flight.
    /// `safety` supplies each file's tier from the classifier.
    ///
    /// # Errors
    ///
    /// As [`Selection::marked_files`].
    pub fn to_queue_items(
        &self,
        report: &DuplicateReport,
        safety: impl Fn(&DupFile) -> Safety,
    ) -> Result<Vec<QueueItem>, SelectionError> {
        Ok(self
            .marked_files(report)?
            .into_iter()
            .map(|(group, index, f)| QueueItem {
                id: queue_item_id(group, index),
                path: f.path.clone(),
                expected: f.expected(),
                safety: safety(f),
            })
            .collect())
    }
}

/// Queue item id for copy `index` of `group`.
///
/// # Example
///
/// ```
/// let id = strata_dupes::queue_item_id(3, 7);
/// assert_eq!(strata_dupes::parse_queue_item_id(id), (3, 7));
/// ```
#[must_use]
pub fn queue_item_id(group: u64, index: usize) -> u64 {
    (group << INDEX_BITS) | (index as u64 & ((1 << INDEX_BITS) - 1))
}

/// Inverse of [`queue_item_id`]: `(group, index)`.
#[must_use]
pub fn parse_queue_item_id(id: u64) -> (u64, usize) {
    (id >> INDEX_BITS, (id & ((1 << INDEX_BITS) - 1)) as usize)
}
