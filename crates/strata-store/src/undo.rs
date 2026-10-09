//! Write-ahead undo and audit log for deletes (SPEC §15.2 step 7, §15.7, §21).
//!
//! Protocol, in order:
//!
//! 1. [`Store::begin_action`] records the action and every planned item with
//!    result `pending`, in one committed transaction on `state.db` (which
//!    runs `synchronous=FULL`, so the commit is fsynced). **Only after it
//!    returns may the cleaner delete anything.**
//! 2. [`Store::complete_item`] records each item's outcome as it happens,
//!    including the Recycle Bin restore information.
//! 3. [`Store::finish_action`] closes the action. Items still `pending` are
//!    marked `skipped`.
//!
//! If the app dies between 1 and 3, the action stays `in_progress`.
//! [`Store::recover_incomplete`] returns such actions on the next launch so
//! the app can check which pending items actually disappeared, record that
//! with `complete_item`, and close the action as
//! [`ActionStatus::Interrupted`].
//!
//! Privileged helper deletes and built-in tool actions are logged the same way
//! (method `tool` for the latter), so this is also the audit log.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime, Safety};

use crate::clock::Timestamp;
use crate::error::{Result, StoreError};
use crate::snapshot::VolumeKey;
use crate::{Store, i2u, u2i};

// -----------------------------------------------------------------------------
// Types
// -----------------------------------------------------------------------------

/// Row id of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(pub i64);

/// Row id of one item of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub i64);

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $variant:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vm])* $variant),+
        }

        impl $name {
            /// Stable string stored in the database.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $s),+ }
            }

            fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

string_enum! {
    /// What kind of user action produced the log entry.
    ActionKind {
        /// Cleanup queue execution.
        Cleanup = "cleanup",
        /// Duplicate removal or hardlink replacement.
        Duplicates = "duplicates",
        /// Built-in tool action (empty Recycle Bin, DISM, ...).
        Tool = "tool",
    }
}

string_enum! {
    /// Lifecycle state of an action.
    ActionStatus {
        /// Written ahead; work may be under way (or the app crashed).
        InProgress = "in_progress",
        /// Every item succeeded or was skipped by choice.
        Completed = "completed",
        /// Some items failed.
        Partial = "partial",
        /// Nothing succeeded.
        Failed = "failed",
        /// The user cancelled.
        Cancelled = "cancelled",
        /// Closed during crash recovery.
        Interrupted = "interrupted",
    }
}

string_enum! {
    /// How an item was (to be) removed.
    DeleteMethod {
        /// Moved to the Recycle Bin.
        Recycle = "recycle",
        /// Deleted permanently.
        Permanent = "permanent",
        /// Scheduled with `MOVEFILE_DELAY_UNTIL_REBOOT`.
        RebootDelete = "reboot_delete",
        /// Removed by a built-in tool.
        Tool = "tool",
    }
}

string_enum! {
    /// Outcome of one item.
    ItemResult {
        /// Not done yet.
        Pending = "pending",
        /// Removed.
        Done = "done",
        /// Removal failed; see the error.
        Failed = "failed",
        /// Not attempted (deselected, cancelled, or already gone).
        Skipped = "skipped",
    }
}

/// One item the cleaner is about to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedItem {
    /// Full path at planning time.
    pub path: String,
    /// Volume holding it.
    pub volume: VolumeKey,
    /// File reference verified in pre-flight.
    pub file_ref: FileRef,
    /// Size in bytes (allocated, as shown to the user).
    pub size: u64,
    /// Last-write time verified in pre-flight.
    pub mtime: FileTime,
    /// Removal method.
    pub method: DeleteMethod,
    /// Safety tier from the classifier.
    pub tier: Safety,
    /// Rule that classified it, if any.
    pub rule_id: Option<String>,
}

/// What the Shell needs to restore a recycled item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreInfo {
    /// Path the item had before it was recycled.
    pub original_path: String,
    /// Opaque restore data defined by `strata-clean` (e.g. the `$I` record
    /// name or a serialized PIDL). Never interpreted here.
    pub blob: Vec<u8>,
}

/// The outcome reported for one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemOutcome {
    /// Result; `Pending` is rejected.
    pub result: ItemResult,
    /// Error text for failures.
    pub error: Option<String>,
    /// Restore information for recycled items.
    pub restore: Option<RestoreInfo>,
}

/// A logged item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRecord {
    /// Row id.
    pub id: ItemId,
    /// Owning action.
    pub action: ActionId,
    /// Position in the action's item list (0-based).
    pub seq: u32,
    /// The plan.
    pub planned: PlannedItem,
    /// Outcome so far.
    pub result: ItemResult,
    /// Error text, if it failed.
    pub error: Option<String>,
    /// When the outcome was recorded.
    pub completed_at: Option<Timestamp>,
    /// Restore information, for recycled items.
    pub restore: Option<RestoreInfo>,
    /// When it was restored from the Recycle Bin, if it was.
    pub restored_at: Option<Timestamp>,
}

/// One action in the history list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSummary {
    /// Row id.
    pub id: ActionId,
    /// What kind of action.
    pub kind: ActionKind,
    /// Current status.
    pub status: ActionStatus,
    /// When it was written ahead.
    pub started_at: Timestamp,
    /// When it was finished.
    pub finished_at: Option<Timestamp>,
    /// Items planned.
    pub item_count: u64,
    /// Items removed.
    pub done_count: u64,
    /// Items that failed.
    pub failed_count: u64,
    /// Bytes of removed items.
    pub bytes_done: u64,
}

/// An action with all its items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecord {
    /// Summary row.
    pub summary: ActionSummary,
    /// Items in plan order.
    pub items: Vec<ItemRecord>,
}

// -----------------------------------------------------------------------------
// SQL
// -----------------------------------------------------------------------------

const SQL_INSERT_ACTION: &str = "
INSERT INTO actions (kind, status, started_at)
VALUES (?1, 'in_progress', ?2)";

const SQL_INSERT_ITEM: &str = "
INSERT INTO action_items (
    action_id, seq, path, volume_serial, volume_guid, file_ref, size, mtime,
    method, tier, rule_id
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

const SQL_ACTION_STATUS: &str = "
SELECT status FROM actions WHERE id = ?1";

const SQL_COMPLETE_ITEM: &str = "
UPDATE action_items
SET result = ?3, error = ?4, completed_at = ?5, original_path = ?6, restore_blob = ?7
WHERE action_id = ?1 AND seq = ?2";

const SQL_SKIP_PENDING: &str = "
UPDATE action_items
SET result = 'skipped', completed_at = ?2
WHERE action_id = ?1 AND result = 'pending'";

const SQL_FINISH_ACTION: &str = "
UPDATE actions SET status = ?2, finished_at = ?3 WHERE id = ?1";

const SQL_SUMMARY_SELECT: &str = "
SELECT a.id, a.kind, a.status, a.started_at, a.finished_at,
       count(i.id),
       count(CASE WHEN i.result = 'done' THEN 1 END),
       count(CASE WHEN i.result = 'failed' THEN 1 END),
       coalesce(sum(CASE WHEN i.result = 'done' THEN i.size END), 0)
FROM actions AS a
LEFT JOIN action_items AS i ON i.action_id = a.id";

const SQL_ITEM_SELECT: &str = "
SELECT id, action_id, seq, path, volume_serial, volume_guid, file_ref, size, mtime,
       method, tier, rule_id, result, error, completed_at, original_path,
       restore_blob, restored_at
FROM action_items";

const SQL_MARK_RESTORED: &str = "
UPDATE action_items SET restored_at = ?2
WHERE id = ?1 AND method = 'recycle' AND result = 'done' AND restored_at IS NULL";

// -----------------------------------------------------------------------------
// Row mapping
// -----------------------------------------------------------------------------

fn bad(idx: usize, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        format!("unrecognized {what}").into(),
    )
}

fn summary_from_row(r: &Row<'_>) -> rusqlite::Result<ActionSummary> {
    let kind: String = r.get(1)?;
    let status: String = r.get(2)?;
    Ok(ActionSummary {
        id: ActionId(r.get(0)?),
        kind: ActionKind::parse(&kind).ok_or_else(|| bad(1, "action kind"))?,
        status: ActionStatus::parse(&status).ok_or_else(|| bad(2, "action status"))?,
        started_at: Timestamp(r.get(3)?),
        finished_at: r.get::<_, Option<i64>>(4)?.map(Timestamp),
        item_count: i2u(r.get(5)?),
        done_count: i2u(r.get(6)?),
        failed_count: i2u(r.get(7)?),
        bytes_done: i2u(r.get(8)?),
    })
}

fn item_from_row(r: &Row<'_>) -> rusqlite::Result<ItemRecord> {
    let method: String = r.get(9)?;
    let tier: String = r.get(10)?;
    let result: String = r.get(12)?;
    let original_path: Option<String> = r.get(15)?;
    let blob: Option<Vec<u8>> = r.get(16)?;
    Ok(ItemRecord {
        id: ItemId(r.get(0)?),
        action: ActionId(r.get(1)?),
        seq: u32::try_from(r.get::<_, i64>(2)?).map_err(|_| bad(2, "sequence"))?,
        planned: PlannedItem {
            path: r.get(3)?,
            volume: VolumeKey {
                serial: i2u(r.get(4)?),
                guid_path: r.get(5)?,
            },
            file_ref: FileRef(i2u(r.get(6)?)),
            size: i2u(r.get(7)?),
            mtime: FileTime(i2u(r.get(8)?)),
            method: DeleteMethod::parse(&method).ok_or_else(|| bad(9, "delete method"))?,
            tier: Safety::from_key(&tier).ok_or_else(|| bad(10, "safety tier"))?,
            rule_id: r.get(11)?,
        },
        result: ItemResult::parse(&result).ok_or_else(|| bad(12, "item result"))?,
        error: r.get(13)?,
        completed_at: r.get::<_, Option<i64>>(14)?.map(Timestamp),
        restore: original_path.map(|original_path| RestoreInfo {
            original_path,
            blob: blob.unwrap_or_default(),
        }),
        restored_at: r.get::<_, Option<i64>>(17)?.map(Timestamp),
    })
}

fn load_action(conn: &Connection, id: ActionId) -> Result<ActionRecord> {
    let summary = conn
        .prepare_cached(&format!(
            "{SQL_SUMMARY_SELECT} WHERE a.id = ?1 GROUP BY a.id"
        ))?
        .query_row([id.0], summary_from_row)
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("action {}", id.0)))?;
    let items = conn
        .prepare_cached(&format!(
            "{SQL_ITEM_SELECT} WHERE action_id = ?1 ORDER BY seq"
        ))?
        .query_map([id.0], item_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ActionRecord { summary, items })
}

fn require_in_progress(conn: &Connection, id: ActionId) -> Result<()> {
    let status: Option<String> = conn
        .prepare_cached(SQL_ACTION_STATUS)?
        .query_row([id.0], |r| r.get(0))
        .optional()?;
    match status.as_deref() {
        None => Err(StoreError::NotFound(format!("action {}", id.0))),
        Some("in_progress") => Ok(()),
        Some(other) => Err(StoreError::InvalidInput(format!(
            "action {} is already {other}",
            id.0
        ))),
    }
}

// -----------------------------------------------------------------------------
// Store API
// -----------------------------------------------------------------------------

impl Store {
    /// Write-ahead step: records the action and its items as `pending` and
    /// commits (fsynced) before returning. Delete nothing until this returns
    /// `Ok`.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_core::{FileRef, FileTime, Safety};
    /// use strata_store::*;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// let item = PlannedItem {
    ///     path: r"C:\Users\me\AppData\Local\Temp\big.tmp".into(),
    ///     volume: VolumeKey { serial: 1, guid_path: r"\\?\Volume{1}\".into() },
    ///     file_ref: FileRef::from_parts(1234, 2),
    ///     size: 4096,
    ///     mtime: FileTime(133_000_000_000_000_000),
    ///     method: DeleteMethod::Recycle,
    ///     tier: Safety::Safe,
    ///     rule_id: Some("temp.user".into()),
    /// };
    /// let id = store.begin_action(ActionKind::Cleanup, &[item]).unwrap();
    /// // ... recycle the file, then:
    /// store.complete_item(id, 0, &ItemOutcome {
    ///     result: ItemResult::Done,
    ///     error: None,
    ///     restore: Some(RestoreInfo { original_path: r"C:\...\big.tmp".into(), blob: vec![1, 2] }),
    /// }).unwrap();
    /// store.finish_action(id, ActionStatus::Completed).unwrap();
    /// assert_eq!(store.restorable_items(10).unwrap().len(), 1);
    /// ```
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidInput`] for an empty item list, or database
    /// errors (in which case nothing was recorded and nothing may be deleted).
    pub fn begin_action(&self, kind: ActionKind, items: &[PlannedItem]) -> Result<ActionId> {
        if items.is_empty() {
            return Err(StoreError::InvalidInput("an action needs items".into()));
        }
        let now = self.now();
        self.state().write(|tx| {
            tx.prepare_cached(SQL_INSERT_ACTION)?
                .execute(params![kind.as_str(), now.0])?;
            let id = tx.last_insert_rowid();
            let mut insert = tx.prepare_cached(SQL_INSERT_ITEM)?;
            for (seq, it) in items.iter().enumerate() {
                insert.execute(params![
                    id,
                    seq as i64,
                    it.path,
                    u2i(it.volume.serial),
                    it.volume.guid_path,
                    u2i(it.file_ref.0),
                    u2i(it.size),
                    u2i(it.mtime.0),
                    it.method.as_str(),
                    it.tier.as_str(),
                    it.rule_id,
                ])?;
            }
            Ok(ActionId(id))
        })
    }

    /// Records the outcome of item `seq` (its index in the `begin_action`
    /// list).
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown action or item,
    /// [`StoreError::InvalidInput`] when the action is already finished or the
    /// result is `Pending`.
    pub fn complete_item(&self, id: ActionId, seq: u32, outcome: &ItemOutcome) -> Result<()> {
        if outcome.result == ItemResult::Pending {
            return Err(StoreError::InvalidInput(
                "an outcome cannot be pending".into(),
            ));
        }
        let now = self.now();
        self.state().write(|tx| {
            require_in_progress(tx, id)?;
            let changed = tx.prepare_cached(SQL_COMPLETE_ITEM)?.execute(params![
                id.0,
                i64::from(seq),
                outcome.result.as_str(),
                outcome.error,
                now.0,
                outcome.restore.as_ref().map(|r| &r.original_path),
                outcome.restore.as_ref().map(|r| &r.blob),
            ])?;
            if changed == 0 {
                return Err(StoreError::NotFound(format!(
                    "item {seq} of action {}",
                    id.0
                )));
            }
            Ok(())
        })
    }

    /// Closes an action. Items still pending become `skipped`.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidInput`] for `InProgress` or an already finished
    /// action, [`StoreError::NotFound`] for an unknown one.
    pub fn finish_action(&self, id: ActionId, status: ActionStatus) -> Result<()> {
        if status == ActionStatus::InProgress {
            return Err(StoreError::InvalidInput(
                "finish_action needs a final status".into(),
            ));
        }
        let now = self.now();
        self.state().write(|tx| {
            require_in_progress(tx, id)?;
            tx.prepare_cached(SQL_SKIP_PENDING)?
                .execute(params![id.0, now.0])?;
            tx.prepare_cached(SQL_FINISH_ACTION)?
                .execute(params![id.0, status.as_str(), now.0])?;
            Ok(())
        })
    }

    /// Actions left `in_progress` by a crash, oldest first, with all items.
    /// Call on startup and reconcile each one.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn recover_incomplete(&self) -> Result<Vec<ActionRecord>> {
        self.state().read(|c| {
            let ids = c
                .prepare_cached("SELECT id FROM actions WHERE status = 'in_progress' ORDER BY id")?
                .query_map([], |r| r.get(0).map(ActionId))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids.into_iter().map(|id| load_action(c, id)).collect()
        })
    }

    /// One action with its items.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown action.
    pub fn action(&self, id: ActionId) -> Result<ActionRecord> {
        self.state().read(|c| load_action(c, id))
    }

    /// History list, newest first. Pass the last id of a page as `before` to
    /// get the next page.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn action_history(
        &self,
        limit: usize,
        before: Option<ActionId>,
    ) -> Result<Vec<ActionSummary>> {
        self.state().read(|c| {
            let rows = c
                .prepare_cached(&format!(
                    "{SQL_SUMMARY_SELECT} WHERE a.id < ?1 GROUP BY a.id ORDER BY a.id DESC LIMIT ?2"
                ))?
                .query_map(
                    params![
                        before.map_or(i64::MAX, |b| b.0),
                        i64::try_from(limit).unwrap_or(i64::MAX)
                    ],
                    summary_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Recycled items not yet restored, newest first.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn restorable_items(&self, limit: usize) -> Result<Vec<ItemRecord>> {
        self.state().read(|c| {
            let rows = c
                .prepare_cached(&format!(
                    "{SQL_ITEM_SELECT}
                     WHERE method = 'recycle' AND result = 'done' AND restored_at IS NULL
                     ORDER BY completed_at DESC, id DESC
                     LIMIT ?1"
                ))?
                .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], item_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Records that a recycled item was restored.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when the item does not exist or is not a
    /// restorable recycled item.
    pub fn mark_restored(&self, item: ItemId) -> Result<()> {
        let now = self.now();
        self.state().write(|tx| {
            let changed = tx
                .prepare_cached(SQL_MARK_RESTORED)?
                .execute(params![item.0, now.0])?;
            if changed == 0 {
                return Err(StoreError::NotFound(format!("restorable item {}", item.0)));
            }
            Ok(())
        })
    }
}
