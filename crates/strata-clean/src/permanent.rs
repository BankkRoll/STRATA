//! Permanent delete by handle.
//!
//! 1. Open the item itself with `DELETE` and `FILE_FLAG_OPEN_REPARSE_POINT`
//!    (a link is deleted, never its target) and run every guard check on
//!    that handle.
//! 2. Verify the scan's file id, kind, size and mtime against the handle.
//! 3. Directories: list children through the directory handle and open each
//!    one *relative to that handle* (`NtCreateFile` with `RootDirectory`),
//!    again without following reparse points, and re-check each child's id
//!    against the listing. A junction or symlink child is unlinked itself.
//!    Directories are held without `FILE_SHARE_DELETE` so nobody can rename
//!    or replace them mid-walk. The walk is iterative, so deep trees cannot
//!    overflow the stack.
//! 4. Delete with `FileDispositionInfoEx` (POSIX semantics, ignore
//!    read-only), falling back to classic disposition.
//!
//! On the first child failure the walk stops and reports
//! [`CleanError::Partial`] rather than leaving scattered holes.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::canon::{CanonicalPath, Name};
use crate::error::{Change, CleanError};
use crate::expect::{CancelToken, Expected};
use crate::guard::{CheckedItem, SafetyGuard};
use crate::win::handle::{
    self, ACCESS_DELETE, ACCESS_LIST_DIRECTORY, ACCESS_READ_ATTRIBUTES, ACCESS_SYNCHRONIZE,
    OwnedHandle, SHARE_ALL, SHARE_NO_DELETE,
};
use crate::win::ntdir::{self, DirEntry};

/// What a delete removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeleteStats {
    /// Files removed.
    pub files: u64,
    /// Directories removed.
    pub dirs: u64,
    /// Links (symlinks, junctions, other reparse points) unlinked.
    pub links: u64,
    /// Logical bytes of removed files.
    pub bytes: u64,
}

impl DeleteStats {
    fn items(&self) -> u64 {
        self.files + self.dirs + self.links
    }
}

const ATTR_DIR: u32 = strata_core::win32::FILE_ATTRIBUTE_DIRECTORY;
const ATTR_REPARSE: u32 = strata_core::win32::FILE_ATTRIBUTE_REPARSE_POINT;

/// Permanently deletes `path` after verifying it is the item the scan saw.
///
/// # Errors
///
/// A typed [`CleanError`]; nothing is deleted unless every check passes.
pub fn delete_permanently(
    guard: &SafetyGuard,
    path: &Path,
    expected: &Expected,
    cancel: &CancelToken,
) -> Result<DeleteStats, CleanError> {
    let display = path.display().to_string();
    if cancel.is_cancelled() {
        return Err(CleanError::Cancelled { path: display });
    }
    let mut access = ACCESS_DELETE | ACCESS_READ_ATTRIBUTES | ACCESS_SYNCHRONIZE;
    if expected.is_dir {
        access |= ACCESS_LIST_DIRECTORY;
    }
    let share = if expected.is_dir {
        SHARE_NO_DELETE
    } else {
        SHARE_ALL
    };
    let (h, item) = guard.open_checked(path, access, share)?;
    expected
        .verify(&item.facts)
        .map_err(|change| CleanError::Changed {
            path: display.clone(),
            change,
        })?;
    delete_verified(guard, h, &item, &display, cancel)
}

/// Deletes an already opened and verified object. Shared with the
/// privileged (by-file-id) path.
pub(crate) fn delete_verified(
    guard: &SafetyGuard,
    h: OwnedHandle,
    item: &CheckedItem,
    display: &str,
    cancel: &CancelToken,
) -> Result<DeleteStats, CleanError> {
    let mut stats = DeleteStats::default();
    let attributes = item.facts.attributes;
    let is_dir = attributes & ATTR_DIR != 0;
    let is_link = attributes & ATTR_REPARSE != 0;
    if is_dir && !is_link {
        delete_contents(guard, &h, &item.resolved, display, cancel, &mut stats)?;
    }
    handle::delete_by_handle(&h).map_err(|e| wrap_partial(display, &stats, &e))?;
    drop(h);
    if is_link {
        stats.links += 1;
    } else if is_dir {
        stats.dirs += 1;
    } else {
        stats.files += 1;
        stats.bytes += item.facts.size;
    }
    Ok(stats)
}

fn wrap_partial(display: &str, stats: &DeleteStats, e: &std::io::Error) -> CleanError {
    let err = CleanError::from_io(display, e);
    partial(display, stats, err)
}

fn partial(display: &str, stats: &DeleteStats, err: CleanError) -> CleanError {
    if stats.items() == 0 {
        err
    } else {
        CleanError::Partial {
            path: display.to_string(),
            deleted: stats.items(),
            first_error: Box::new(err),
        }
    }
}

fn join_display(parent: &str, name: &[u16]) -> String {
    format!("{parent}\\{}", String::from_utf16_lossy(name))
}

struct Frame {
    /// `None` for the root, which the caller owns.
    handle: Option<OwnedHandle>,
    display: String,
    canon: CanonicalPath,
    children: std::vec::IntoIter<DirEntry>,
}

fn delete_contents(
    guard: &SafetyGuard,
    root: &OwnedHandle,
    root_canon: &CanonicalPath,
    display: &str,
    cancel: &CancelToken,
    stats: &mut DeleteStats,
) -> Result<(), CleanError> {
    let list = |h: &OwnedHandle, at: &str, stats: &DeleteStats| {
        ntdir::list_dir(h).map_err(|e| wrap_partial(at, stats, &e))
    };
    let mut stack = vec![Frame {
        handle: None,
        display: display.to_string(),
        canon: root_canon.clone(),
        children: list(root, display, stats)?.into_iter(),
    }];
    loop {
        let Some(top) = stack.last_mut() else {
            return Ok(());
        };
        let Some(entry) = top.children.next() else {
            let done = stack.pop().expect("non-empty");
            if let Some(h) = done.handle {
                handle::delete_by_handle(&h).map_err(|e| wrap_partial(&done.display, stats, &e))?;
                stats.dirs += 1;
            }
            continue;
        };
        if cancel.is_cancelled() {
            return Err(partial(
                display,
                stats,
                CleanError::Cancelled {
                    path: display.to_string(),
                },
            ));
        }
        let top = stack.last().expect("non-empty");
        let parent_h = top.handle.as_ref().unwrap_or(root);
        let child_display = join_display(&top.display, &entry.name);
        let child_canon = top.canon.join(Name::from_units(&entry.name));

        // SECURITY: re-check every child against the never-list, even though
        // the top-level item already proved it contains nothing protected.
        if let Err(refusal) = guard.never_list().check_as(&child_canon, &child_display) {
            return Err(partial(display, stats, refusal.into()));
        }

        let is_dir = entry.attributes & ATTR_DIR != 0;
        let is_link = entry.attributes & ATTR_REPARSE != 0;
        let descend = is_dir && !is_link;
        let mut access = ACCESS_DELETE | ACCESS_READ_ATTRIBUTES;
        if descend {
            access |= ACCESS_LIST_DIRECTORY;
        }
        let share = if descend { SHARE_NO_DELETE } else { SHARE_ALL };
        let child = ntdir::open_relative(parent_h, &entry.name, access, share)
            .map_err(|e| wrap_partial(&child_display, stats, &e))?;
        let info = handle::info(&child).map_err(|e| wrap_partial(&child_display, stats, &e))?;
        // SECURITY: the name may have been swapped between listing and open;
        // only delete the object the listing described.
        let same_id = entry.file_id == 0 || info.file_index == entry.file_id;
        let same_kind = (info.attributes & (ATTR_DIR | ATTR_REPARSE))
            == (entry.attributes & (ATTR_DIR | ATTR_REPARSE));
        if !same_id || !same_kind {
            return Err(partial(
                display,
                stats,
                CleanError::Changed {
                    path: child_display,
                    change: Change::Identity {
                        expected: strata_core::FileRef(entry.file_id),
                        found: info.file_id,
                    },
                },
            ));
        }
        if let Err(e) = guard.check_identity(&info, &child_display) {
            return Err(partial(display, stats, e));
        }
        if descend {
            let children = list(&child, &child_display, stats)?;
            stack.push(Frame {
                handle: Some(child),
                display: child_display,
                canon: child_canon,
                children: children.into_iter(),
            });
        } else {
            handle::delete_by_handle(&child)
                .map_err(|e| wrap_partial(&child_display, stats, &e))?;
            if is_link {
                stats.links += 1;
            } else {
                stats.files += 1;
                stats.bytes += info.size;
            }
        }
    }
}
