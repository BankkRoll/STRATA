//! Per-tick coalescing of journal records into refresh and remove sets.
//!
//! Every file reference appears at most once, however many records name it,
//! so a file written 10,000 times in a tick is fetched once. References keep
//! the order of their first record, so a backlog drains oldest first.

use std::collections::{HashMap, VecDeque};

use strata_core::FileRef;

use crate::reason;

#[derive(Debug, Clone, Copy, Default)]
struct Pending {
    /// The file's last name is gone: remove it without fetching.
    deleted: bool,
    /// A rename's old-name record arrived without its new-name record.
    rename_open: bool,
    /// Epoch in which the rename opened.
    rename_epoch: u64,
}

/// What one batch should do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Deleted files.
    pub remove: Vec<FileRef>,
    /// Files to re-read.
    pub refresh: Vec<FileRef>,
}

impl Plan {
    pub fn len(&self) -> usize {
        self.remove.len() + self.refresh.len()
    }

    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.refresh.is_empty()
    }
}

/// Outcome of noting one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Noted {
    /// The record carries no state change.
    Ignored,
    /// The file is (or already was) scheduled.
    Scheduled,
    /// The record completed a rename whose old half was pending.
    PairedRename,
}

/// Dirty set of one tick (and any backlog carried over).
#[derive(Debug, Default)]
pub(crate) struct Coalescer {
    map: HashMap<FileRef, Pending>,
    order: VecDeque<FileRef>,
    /// Incremented after every flush, so holding a rename spans whole ticks
    /// however many batches one tick takes.
    epoch: u64,
}

impl Coalescer {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    /// Entries that are ready now (not held back for a rename partner).
    pub fn ready(&self) -> usize {
        self.map.values().filter(|p| !p.rename_open).count()
    }

    /// Marks the end of a tick.
    pub fn advance_epoch(&mut self) {
        self.epoch += 1;
    }

    fn entry(&mut self, file: FileRef) -> &mut Pending {
        let order = &mut self.order;
        self.map.entry(file).or_insert_with(|| {
            order.push_back(file);
            Pending::default()
        })
    }

    /// Records one journal record for `file` with accumulated `reason` bits.
    pub fn note(&mut self, file: FileRef, reason: u32) -> Noted {
        if !reason::needs_refresh(reason) {
            return Noted::Ignored;
        }
        let epoch = self.epoch;
        let p = self.entry(file);
        if reason::is_delete(reason) {
            p.deleted = true;
        }
        let mut out = Noted::Scheduled;
        if reason::closes_rename(reason) {
            if p.rename_open {
                out = Noted::PairedRename;
            }
            p.rename_open = false;
        } else if reason::opens_rename(reason) {
            p.rename_open = true;
            p.rename_epoch = epoch;
        }
        if p.deleted {
            p.rename_open = false;
        }
        out
    }

    /// Schedules a refresh of `file` without a journal record of its own
    /// (a directory whose name set changed).
    pub fn touch(&mut self, file: FileRef) {
        self.entry(file);
    }

    /// Takes up to `max` ready entries, oldest first. Entries waiting for the
    /// second half of a rename are held back for up to `grace` epochs
    /// (ticks), so a move split across journal reads is applied as one
    /// change.
    pub fn take(&mut self, max: usize, grace: u64) -> Plan {
        let mut plan = Plan::default();
        let mut held = Vec::new();
        let mut scanned = 0;
        let limit = self.order.len();
        while plan.len() < max && scanned < limit {
            let Some(file) = self.order.pop_front() else {
                break;
            };
            scanned += 1;
            let Some(p) = self.map.get_mut(&file) else {
                continue;
            };
            if p.rename_open && self.epoch.saturating_sub(p.rename_epoch) < grace {
                held.push(file);
                continue;
            }
            let deleted = p.deleted;
            self.map.remove(&file);
            if deleted {
                plan.remove.push(file);
            } else {
                plan.refresh.push(file);
            }
        }
        for file in held.into_iter().rev() {
            self.order.push_front(file);
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_ntfs::usn::*;

    fn r(n: u64) -> FileRef {
        FileRef::from_parts(n, 1)
    }

    #[test]
    fn coalesces_by_reference_in_first_seen_order() {
        let mut c = Coalescer::default();
        for _ in 0..10_000 {
            c.note(r(7), USN_REASON_DATA_EXTEND);
        }
        c.note(r(3), USN_REASON_FILE_CREATE);
        c.note(r(7), USN_REASON_DATA_EXTEND | USN_REASON_CLOSE);
        assert_eq!(c.len(), 2);
        let plan = c.take(10, 1);
        assert_eq!(plan.refresh, vec![r(7), r(3)]);
        assert!(c.is_empty());
    }

    #[test]
    fn deletes_skip_the_fetch_and_stick() {
        let mut c = Coalescer::default();
        c.note(r(1), USN_REASON_FILE_CREATE);
        c.note(r(1), USN_REASON_FILE_DELETE | USN_REASON_CLOSE);
        c.touch(r(1));
        assert_eq!(
            c.take(10, 1),
            Plan {
                remove: vec![r(1)],
                refresh: vec![]
            }
        );
    }

    #[test]
    fn irrelevant_records_are_ignored() {
        let mut c = Coalescer::default();
        assert_eq!(
            c.note(r(1), USN_REASON_SECURITY_CHANGE | USN_REASON_CLOSE),
            Noted::Ignored
        );
        assert!(c.is_empty());
    }

    #[test]
    fn open_rename_is_held_for_the_grace_period() {
        let mut c = Coalescer::default();
        c.note(r(1), USN_REASON_RENAME_OLD_NAME);
        c.note(r(2), USN_REASON_DATA_EXTEND);
        let plan = c.take(10, 1);
        assert_eq!(plan.refresh, vec![r(2)]);
        // Several takes within one tick keep holding it.
        assert!(c.take(10, 1).is_empty());
        assert_eq!(c.len(), 1);
        assert_eq!(c.ready(), 0);
        c.advance_epoch();
        assert_eq!(
            c.note(r(1), USN_REASON_RENAME_NEW_NAME),
            Noted::PairedRename
        );
        assert_eq!(c.take(10, 1).refresh, vec![r(1)]);

        // Without a partner it is released after the grace period.
        c.note(r(4), USN_REASON_RENAME_OLD_NAME);
        assert!(c.take(10, 1).is_empty());
        c.advance_epoch();
        assert_eq!(c.take(10, 1).refresh, vec![r(4)]);
    }

    #[test]
    fn take_respects_the_limit_and_keeps_order() {
        let mut c = Coalescer::default();
        for i in 0..10 {
            c.note(r(i), USN_REASON_FILE_CREATE);
        }
        assert_eq!(c.take(4, 1).refresh, (0..4).map(r).collect::<Vec<_>>());
        assert_eq!(c.take(4, 1).refresh, (4..8).map(r).collect::<Vec<_>>());
        assert_eq!(c.len(), 2);
    }
}
