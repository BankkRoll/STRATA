//! "Replace duplicates with hardlinks": keep one copy's data and make every
//! other copy's path a hardlink to it.
//!
//! Same volume only, user-confirmed through a
//! [`Consent`](strata_clean::consent::Consent), with [`HardlinkWarning`]s
//! for the UI. For each replaced copy:
//!
//! 1. The keeper is opened **denying writers** (held until the end) and
//!    re-hashed; it must still match the group's BLAKE3, file id, size and
//!    mtime. Nobody can change its content after this point.
//! 2. The copy is opened (no recall), its id, size, mtime and volume are
//!    verified, and it is re-hashed against the group hash.
//! 3. A hardlink to the keeper is created under a temporary name next to
//!    the copy, and its file id is checked to be the keeper's. Nothing has
//!    been removed yet, so a failure here (link limit, permissions, a
//!    filesystem without hardlinks) leaves everything as it was.
//! 4. The copy goes to the Recycle Bin through `strata-clean`'s flow
//!    (never-list, pre-flight TOCTOU check, write-ahead audit log). If that
//!    fails, the temporary link is removed (through `strata-clean`, by id)
//!    and the copy is untouched.
//! 5. The temporary link is renamed onto the copy's name, failing rather
//!    than replacing if something took the name meanwhile. On failure the
//!    copy is restored from the Recycle Bin when its name is still free,
//!    and the temporary link is removed.
//!
//! The original copy stays restorable from the Recycle Bin (the undo log
//! has its ticket); restoring it requires removing the link first, which
//! [`HardlinkWarning::RestoreNeedsLinkRemoved`] tells the user.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use strata_clean::audit::{AuditLog, ItemOutcome};
use strata_clean::consent::{Consent, ConsentAction, ConsentError};
use strata_clean::flow::{self, CleanupConfig, Decision, DeleteMethod, QueueItem};
use strata_clean::permanent::delete_permanently;
use strata_clean::recycle::{self, RestoreTicket};
use strata_clean::{CancelToken, CleanError, Expected, SafetyGuard};
use strata_core::Safety;

use crate::gate::SkipReason;
use crate::hash::{self, Expect, HashFail};
use crate::report::{DupFile, DuplicateReport};
use crate::select::{Selection, SelectionError, queue_item_id};
use crate::throttle::Throttle;
use crate::win::{self, Access, Facts};

/// NTFS allows 1024 names per file.
pub const MAX_LINKS: u32 = 1024;

/// Things the user must understand before confirming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardlinkWarning {
    /// All paths become one file: editing any of them changes all.
    EditingOneChangesAll,
    /// Attributes, timestamps and permissions are shared too.
    SharedMetadata,
    /// Programs that save by writing a new file and renaming it break the
    /// link silently; the copies then diverge again (and use space again).
    SaveByReplaceBreaksLinks,
    /// Restoring an original from the Recycle Bin needs the link at its
    /// path removed first.
    RestoreNeedsLinkRemoved,
    /// Backup and sync tools may still copy or upload each path separately.
    BackupToolsSeeEveryPath,
}

impl HardlinkWarning {
    /// Every warning, in display order.
    pub const ALL: [Self; 5] = [
        Self::EditingOneChangesAll,
        Self::SharedMetadata,
        Self::SaveByReplaceBreaksLinks,
        Self::RestoreNeedsLinkRemoved,
        Self::BackupToolsSeeEveryPath,
    ];

    /// Text for the UI.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::EditingOneChangesAll => {
                "These paths will all point to the same data: editing any one of them changes all of them."
            }
            Self::SharedMetadata => {
                "Attributes, timestamps and permissions become shared by every path."
            }
            Self::SaveByReplaceBreaksLinks => {
                "Apps that save by replacing the file will split it into a separate copy again."
            }
            Self::RestoreNeedsLinkRemoved => {
                "The replaced copies go to the Recycle Bin; to restore one, delete the link at its path first."
            }
            Self::BackupToolsSeeEveryPath => {
                "Backup and sync tools may still treat each path as a separate file."
            }
        }
    }
}

/// The confirmed action: link every path in `replace` to `keeper`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceWithHardlinks {
    /// Group id.
    pub group: u64,
    /// Content hash all copies must still have.
    pub hash: [u8; 32],
    /// The copy whose data is kept.
    pub keeper: DupFile,
    /// Copies to replace, with their file index in the group.
    pub replace: Vec<(usize, DupFile)>,
    /// Bytes freed when every copy is replaced.
    pub bytes_saved: u64,
}

impl ConsentAction for ReplaceWithHardlinks {
    fn describe(&self) -> String {
        let mut s = format!(
            "Replace {} duplicate(s) of {} with hardlinks, freeing {} bytes?\n",
            self.replace.len(),
            self.keeper.path.display(),
            self.bytes_saved
        );
        for (_, f) in &self.replace {
            s.push_str(&format!("  {}\n", f.path.display()));
        }
        for w in HardlinkWarning::ALL {
            s.push_str("- ");
            s.push_str(w.message());
            s.push('\n');
        }
        s
    }
}

/// Why a group cannot be replaced with hardlinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HardlinkRefusal {
    /// The selection does not fit the report.
    #[error(transparent)]
    Selection(#[from] SelectionError),
    /// No copy of the group is marked.
    #[error("select the copies to replace first")]
    NothingMarked,
    /// A marked copy is on another volume than the keeper.
    #[error("{path} is on a different drive; hardlinks only work within one drive")]
    DifferentVolume {
        /// The copy.
        path: PathBuf,
    },
    /// The keeper would exceed the filesystem's link limit.
    #[error("the file would have {links} names; the limit is {MAX_LINKS}")]
    TooManyLinks {
        /// Names the keeper would end up with.
        links: u64,
    },
}

/// Builds the action for one group from the selection: the group's keeper
/// keeps its data and every marked copy becomes a link to it.
///
/// # Errors
///
/// A [`HardlinkRefusal`].
pub fn plan(
    report: &DuplicateReport,
    selection: &Selection,
    group: u64,
) -> Result<ReplaceWithHardlinks, HardlinkRefusal> {
    let g = report
        .group(group)
        .ok_or(SelectionError::UnknownGroup { group })?;
    let sel = selection
        .group(group)
        .ok_or(SelectionError::UnknownGroup { group })?;
    let replace: Vec<(usize, DupFile)> = selection
        .marked_files(report)?
        .into_iter()
        .filter(|(gid, _, _)| *gid == group)
        .map(|(_, i, f)| (i, f.clone()))
        .collect();
    if replace.is_empty() {
        return Err(HardlinkRefusal::NothingMarked);
    }
    let keeper = g.files[sel.keeper()].clone();
    if let Some((_, f)) = replace
        .iter()
        .find(|(_, f)| f.volume_serial != keeper.volume_serial)
    {
        return Err(HardlinkRefusal::DifferentVolume {
            path: f.path.clone(),
        });
    }
    let links = u64::from(keeper.links) + replace.len() as u64;
    if links > u64::from(MAX_LINKS) {
        return Err(HardlinkRefusal::TooManyLinks { links });
    }
    Ok(ReplaceWithHardlinks {
        group,
        hash: g.hash,
        bytes_saved: g.size.saturating_mul(replace.len() as u64),
        keeper,
        replace,
    })
}

/// Why one copy was not replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkError {
    /// The keeper changed, vanished or cannot be read.
    #[error("the kept copy cannot be verified: {reason:?}")]
    Keeper {
        /// Details.
        reason: SkipReason,
    },
    /// The copy changed, vanished or cannot be read.
    #[error("the copy cannot be verified: {reason:?}")]
    Copy {
        /// Details.
        reason: SkipReason,
    },
    /// The copy's content no longer matches.
    #[error("the copy's content changed")]
    ContentDiffers,
    /// The copy is on another volume than the keeper.
    #[error("the copy is on a different drive")]
    DifferentVolume,
    /// Creating the hardlink failed; nothing was changed.
    #[error("could not create the hardlink: {message}")]
    CreateLink {
        /// OS message.
        message: String,
    },
    /// `strata-clean` did not recycle the copy; nothing was changed.
    #[error("the copy was not moved to the Recycle Bin: {error}")]
    Recycle {
        /// The cleaner's reason.
        error: CleanError,
    },
    /// The copy was recycled but the link could not take its name.
    #[error("the link could not take the copy's name ({message}); restored: {restored}")]
    Rename {
        /// OS message.
        message: String,
        /// Whether the copy was restored from the Recycle Bin.
        restored: bool,
    },
    /// Cancelled before this copy was touched.
    #[error("cancelled")]
    Cancelled,
}

/// Result for one copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkOutcome {
    /// File index in the group.
    pub index: usize,
    /// The copy's path.
    pub path: PathBuf,
    /// The Recycle Bin ticket of the replaced copy, or why it failed.
    pub result: Result<RestoreTicket, LinkError>,
}

/// Tunables for [`replace_with_hardlinks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkConfig {
    /// Bytes per read while re-hashing.
    pub read_buffer: usize,
    /// Combined read limit in bytes per second (`None` = unlimited).
    pub max_bytes_per_sec: Option<u64>,
    /// See [`crate::ScanConfig::allow_wof_and_dedup`].
    pub allow_wof_and_dedup: bool,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            read_buffer: 1024 * 1024,
            max_bytes_per_sec: None,
            allow_wof_and_dedup: true,
        }
    }
}

/// Runs a confirmed [`ReplaceWithHardlinks`]. See the module docs for the
/// steps. Each copy is one `strata-clean` action in the audit log.
///
/// `safety` supplies each copy's classifier tier. "Careful" copies are
/// acknowledged by the consent itself, which listed them by path;
/// "never" copies are refused by the cleaner.
///
/// # Errors
///
/// [`ConsentError`] when the confirmation expired. Per-copy failures are
/// reported in the outcomes.
pub fn replace_with_hardlinks(
    guard: &SafetyGuard,
    consent: Consent<ReplaceWithHardlinks>,
    safety: impl Fn(&DupFile) -> Safety,
    audit: &mut dyn AuditLog,
    cfg: &LinkConfig,
    cancel: &CancelToken,
) -> Result<Vec<LinkOutcome>, ConsentError> {
    let action = consent.redeem()?;
    let throttle = Throttle::new(cfg.max_bytes_per_sec);
    let mut buf = vec![0u8; cfg.read_buffer.max(64 * 1024)];
    let outcome = |index: usize, f: &DupFile, result| LinkOutcome {
        index,
        path: f.path.clone(),
        result,
    };

    let expect = |f: &DupFile| Expect {
        file_ref: f.file_ref,
        state: Some((f.size, f.mtime)),
        allow_wof_and_dedup: cfg.allow_wof_and_dedup,
    };
    // Held until every copy is done: denies writers, so the data every new
    // link points at is the data that was just hashed.
    let keeper = match hash::open_checked(
        &action.keeper.path,
        Access::ReadDenyWrite,
        &expect(&action.keeper),
    )
    .map_err(HashFail::Skip)
    .and_then(|(file, facts)| {
        let h = hash::full_hash(&file, &facts, &mut buf, &throttle, cancel, &|_| {})?;
        Ok((file, facts, h))
    }) {
        Ok((file, facts, h)) if h == action.hash => (file, facts),
        Ok(_) => {
            return Ok(fail_all(&action, || LinkError::Keeper {
                reason: SkipReason::Changed,
            }));
        }
        Err(HashFail::Cancelled) => return Ok(fail_all(&action, || LinkError::Cancelled)),
        Err(HashFail::Skip(reason)) => {
            return Ok(fail_all(&action, || LinkError::Keeper {
                reason: reason.clone(),
            }));
        }
    };

    let mut out = Vec::with_capacity(action.replace.len());
    for (index, copy) in &action.replace {
        if cancel.is_cancelled() {
            out.push(outcome(*index, copy, Err(LinkError::Cancelled)));
            continue;
        }
        let result = replace_one(
            guard,
            &action,
            &keeper.1,
            *index,
            copy,
            &safety(copy),
            audit,
            &expect(copy),
            &mut buf,
            &throttle,
            cancel,
        );
        out.push(outcome(*index, copy, result));
    }
    drop(keeper);
    Ok(out)
}

fn fail_all(action: &ReplaceWithHardlinks, e: impl Fn() -> LinkError) -> Vec<LinkOutcome> {
    action
        .replace
        .iter()
        .map(|(i, f)| LinkOutcome {
            index: *i,
            path: f.path.clone(),
            result: Err(e()),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn replace_one(
    guard: &SafetyGuard,
    action: &ReplaceWithHardlinks,
    keeper: &Facts,
    index: usize,
    copy: &DupFile,
    safety: &Safety,
    audit: &mut dyn AuditLog,
    expect: &Expect,
    buf: &mut [u8],
    throttle: &Throttle,
    cancel: &CancelToken,
) -> Result<RestoreTicket, LinkError> {
    // 2. Verify the copy.
    {
        let (file, facts) = hash::open_checked(&copy.path, Access::Read, expect)
            .map_err(|reason| LinkError::Copy { reason })?;
        if facts.volume_serial != keeper.volume_serial {
            return Err(LinkError::DifferentVolume);
        }
        match hash::full_hash(&file, &facts, buf, throttle, cancel, &|_| {}) {
            Ok(h) if h == action.hash => {}
            Ok(_) => return Err(LinkError::ContentDiffers),
            Err(HashFail::Cancelled) => return Err(LinkError::Cancelled),
            Err(HashFail::Skip(reason)) => return Err(LinkError::Copy { reason }),
        }
    }

    // 3. Link under a temporary name.
    let temp = temp_name(&copy.path).ok_or_else(|| LinkError::CreateLink {
        message: "the copy has no parent folder".into(),
    })?;
    std::fs::hard_link(&action.keeper.path, &temp).map_err(|e| LinkError::CreateLink {
        message: e.to_string(),
    })?;
    let link_expected = Expected {
        file_ref: action.keeper.file_ref,
        is_dir: false,
        size: keeper.size,
        modified: keeper.mtime,
    };
    let linked_ok = win::open(&temp, Access::Metadata)
        .and_then(|f| win::facts(&f))
        .is_ok_and(|f| f.index == action.keeper.file_ref.0);
    if !linked_ok {
        remove_temp(guard, &temp, &link_expected);
        return Err(LinkError::CreateLink {
            message: "the new link does not resolve to the kept file".into(),
        });
    }

    // 4. Recycle the copy through the cleaner.
    let id = queue_item_id(action.group, index);
    let item = QueueItem {
        id,
        path: copy.path.clone(),
        expected: copy.expected(),
        safety: *safety,
    };
    let plan = flow::plan(guard, vec![item]);
    let mut decision = Decision {
        method: DeleteMethod::RecycleBin,
        ..Decision::recycle()
    };
    decision.acks.careful.insert(id);
    let report = flow::execute(
        guard,
        &plan,
        &decision,
        &CleanupConfig::default(),
        audit,
        &mut |_| {},
        cancel,
    );
    let recycled = report
        .results
        .into_iter()
        .find(|r| r.id == id)
        .map(|r| r.outcome);
    let ticket = match recycled {
        Some(ItemOutcome::Recycled { ticket }) => ticket,
        other => {
            remove_temp(guard, &temp, &link_expected);
            let error = match other {
                Some(ItemOutcome::Failed { error } | ItemOutcome::Skipped { reason: error }) => {
                    error
                }
                _ => plan_refusal(&plan, &copy.path),
            };
            return Err(LinkError::Recycle { error });
        }
    };

    // 5. Move the link onto the copy's name.
    if let Err(e) = win::rename_no_replace(&temp, &copy.path) {
        let restored = recycle::restore(&ticket).is_ok();
        remove_temp(guard, &temp, &link_expected);
        return Err(LinkError::Rename {
            message: e.to_string(),
            restored,
        });
    }
    Ok(ticket)
}

fn plan_refusal(plan: &flow::Plan, path: &Path) -> CleanError {
    plan.warnings
        .iter()
        .find_map(|w| match w {
            flow::PlanWarning::Refused { refusal, .. } => Some(CleanError::Refused {
                refusal: refusal.clone(),
            }),
            _ => None,
        })
        .unwrap_or_else(|| CleanError::NotFound {
            path: path.display().to_string(),
        })
}

/// Removes a temporary link we created, by id, through the cleaner. The
/// file keeps its other names, so only this name goes away.
fn remove_temp(guard: &SafetyGuard, temp: &Path, expected: &Expected) {
    // Best effort: the outcome already reports the real failure, and a
    // leftover temp link costs no space (it is the kept file).
    let _ = delete_permanently(guard, temp, expected, &CancelToken::new());
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_name(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let name = path.file_name()?;
    let mut n = OsString::from(".");
    n.push(name);
    n.push(format!(
        ".strata-link-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    Some(parent.join(n))
}
