//! The undo/audit log contract (SPEC §15.2 step 7, §15.7, §21).
//!
//! Write-ahead: [`crate::flow::execute`] calls
//! [`AuditLog::begin_action`] once, then [`AuditLog::item_started`] for an
//! item **before** touching it, then [`AuditLog::item_finished`] with the
//! outcome, and finally [`AuditLog::finish_action`]. If `item_started`
//! fails, that item is not acted on. After a crash, an item with a start
//! record but no finish record was possibly acted on: the store should mark
//! it "interrupted" and offer [`crate::recycle::find_in_recycle_bin`] to
//! recover a Restore ticket. Items skipped before acting get a finish
//! record without a start record.
//!
//! The SQLite store (`strata-store`) implements this trait through an
//! adapter in the app. [`MemoryAuditLog`] is an in-memory implementation
//! for tests and for running before the store is wired up.

use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime, Safety};

use crate::error::CleanError;
use crate::permanent::DeleteStats;
use crate::recycle::RestoreTicket;

/// How items are removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMethod {
    /// Move to the Recycle Bin (default).
    RecycleBin,
    /// Delete permanently.
    Permanent,
}

/// Identifier the log assigns to one cleanup action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(pub u64);

/// One item as recorded in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditItem {
    /// Caller's queue item id.
    pub item_id: u64,
    /// Path as queued.
    pub path: String,
    /// File reference from the scan.
    pub file_ref: FileRef,
    /// Size from the scan.
    pub size: u64,
    /// Safety tier.
    pub safety: Safety,
}

/// A cleanup action as recorded before it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditAction {
    /// Method chosen by the user.
    pub method: DeleteMethod,
    /// When the action began.
    pub started_at: FileTime,
    /// Every planned item.
    pub items: Vec<AuditItem>,
}

/// What happened to one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemOutcome {
    /// Moved to the Recycle Bin; restorable.
    Recycled {
        /// Store [`RestoreTicket::to_blob`] for Restore.
        ticket: RestoreTicket,
    },
    /// Deleted permanently.
    Deleted {
        /// What was removed.
        stats: DeleteStats,
    },
    /// Attempted and failed; nothing or (see [`CleanError::Partial`]) part
    /// of it was removed.
    Failed {
        /// Why.
        error: CleanError,
    },
    /// Not attempted (gate, acknowledgement, cancellation).
    Skipped {
        /// Why.
        reason: CleanError,
    },
}

impl ItemOutcome {
    /// Whether the item is gone from its original location.
    #[must_use]
    pub fn removed(&self) -> bool {
        matches!(self, Self::Recycled { .. } | Self::Deleted { .. })
    }
}

/// Totals written when an action ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ActionSummary {
    /// Items recycled or deleted.
    pub succeeded: u64,
    /// Items that failed.
    pub failed: u64,
    /// Items skipped.
    pub skipped: u64,
    /// Bytes freed (scan sizes of removed items).
    pub bytes: u64,
    /// Whether the user cancelled.
    pub cancelled: bool,
}

/// The log could not be written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("the undo log could not be written: {0}")]
pub struct AuditError(pub String);

/// Write-ahead audit log. See the module docs for the call order.
pub trait AuditLog {
    /// Records a new action before any item is touched.
    ///
    /// # Errors
    ///
    /// When the record cannot be persisted; nothing is acted on then.
    fn begin_action(&mut self, action: &AuditAction) -> Result<ActionId, AuditError>;

    /// Records that `item` is about to be acted on. Must be durable before
    /// returning.
    ///
    /// # Errors
    ///
    /// When the record cannot be persisted; the item is then not touched.
    fn item_started(&mut self, action: ActionId, item: &AuditItem) -> Result<(), AuditError>;

    /// Records an item's outcome.
    ///
    /// # Errors
    ///
    /// When the record cannot be persisted (the action continues; the start
    /// record already marks the item).
    fn item_finished(
        &mut self,
        action: ActionId,
        item_id: u64,
        outcome: &ItemOutcome,
    ) -> Result<(), AuditError>;

    /// Records the end of the action.
    ///
    /// # Errors
    ///
    /// When the record cannot be persisted.
    fn finish_action(
        &mut self,
        action: ActionId,
        summary: &ActionSummary,
    ) -> Result<(), AuditError>;
}

/// One entry of a [`MemoryAuditLog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    /// `begin_action`.
    Begin(ActionId, AuditAction),
    /// `item_started`.
    Started(ActionId, AuditItem),
    /// `item_finished`.
    Finished(ActionId, u64, ItemOutcome),
    /// `finish_action`.
    End(ActionId, ActionSummary),
}

/// In-memory [`AuditLog`] for tests and for running without the store.
#[derive(Debug, Clone, Default)]
pub struct MemoryAuditLog {
    /// Every call, in order.
    pub events: Vec<AuditEvent>,
    next: u64,
    /// When set, `item_started` fails for this item id (exercises the
    /// write-ahead guarantee).
    pub fail_start_for: Option<u64>,
}

impl MemoryAuditLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Items with a start record but no finish record: what a crash
    /// recovery pass would mark "interrupted".
    #[must_use]
    pub fn interrupted(&self) -> Vec<(ActionId, u64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                AuditEvent::Started(a, i) => Some((*a, i.item_id)),
                _ => None,
            })
            .filter(|(a, id)| {
                !self
                    .events
                    .iter()
                    .any(|e| matches!(e, AuditEvent::Finished(b, j, _) if b == a && j == id))
            })
            .collect()
    }
}

impl AuditLog for MemoryAuditLog {
    fn begin_action(&mut self, action: &AuditAction) -> Result<ActionId, AuditError> {
        self.next += 1;
        let id = ActionId(self.next);
        self.events.push(AuditEvent::Begin(id, action.clone()));
        Ok(id)
    }

    fn item_started(&mut self, action: ActionId, item: &AuditItem) -> Result<(), AuditError> {
        if self.fail_start_for == Some(item.item_id) {
            return Err(AuditError("simulated write failure".into()));
        }
        self.events.push(AuditEvent::Started(action, item.clone()));
        Ok(())
    }

    fn item_finished(
        &mut self,
        action: ActionId,
        item_id: u64,
        outcome: &ItemOutcome,
    ) -> Result<(), AuditError> {
        self.events
            .push(AuditEvent::Finished(action, item_id, outcome.clone()));
        Ok(())
    }

    fn finish_action(
        &mut self,
        action: ActionId,
        summary: &ActionSummary,
    ) -> Result<(), AuditError> {
        self.events.push(AuditEvent::End(action, *summary));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64) -> AuditItem {
        AuditItem {
            item_id: id,
            path: format!(r"D:\x\{id}"),
            file_ref: FileRef(id),
            size: 1,
            safety: Safety::Safe,
        }
    }

    #[test]
    fn interrupted_items_are_those_started_but_not_finished() {
        let mut log = MemoryAuditLog::new();
        let a = log
            .begin_action(&AuditAction {
                method: DeleteMethod::RecycleBin,
                started_at: FileTime(0),
                items: vec![item(1), item(2)],
            })
            .unwrap();
        log.item_started(a, &item(1)).unwrap();
        log.item_finished(
            a,
            1,
            &ItemOutcome::Deleted {
                stats: DeleteStats::default(),
            },
        )
        .unwrap();
        log.item_started(a, &item(2)).unwrap();
        assert_eq!(log.interrupted(), [(a, 2)]);
    }

    #[test]
    fn simulated_start_failure() {
        let mut log = MemoryAuditLog {
            fail_start_for: Some(7),
            ..Default::default()
        };
        assert!(log.item_started(ActionId(1), &item(7)).is_err());
        assert!(log.item_started(ActionId(1), &item(8)).is_ok());
    }
}
