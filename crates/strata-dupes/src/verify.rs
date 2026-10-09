//! Re-verification right before a delete: each marked copy against the
//! group's keeper, by full hash or byte for byte.

use serde::{Deserialize, Serialize};
use strata_clean::CancelToken;

use crate::gate::SkipReason;
use crate::hash::{self, Expect, HashFail};
use crate::report::{DupFile, DuplicateReport};
use crate::select::{Selection, SelectionError};
use crate::throttle::Throttle;
use crate::win::Access;

/// How thoroughly to re-check before deleting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// Re-hash both copies with BLAKE3 and compare with the group hash.
    FullHash,
    /// Compare the marked copy with the keeper byte for byte.
    ByteCompare,
}

/// Tunables for [`verify_selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyConfig {
    /// Re-check mode.
    pub mode: VerifyMode,
    /// Bytes per read.
    pub read_buffer: usize,
    /// Combined read limit in bytes per second (`None` = unlimited).
    pub max_bytes_per_sec: Option<u64>,
    /// See [`crate::ScanConfig::allow_wof_and_dedup`].
    pub allow_wof_and_dedup: bool,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            mode: VerifyMode::ByteCompare,
            read_buffer: 1024 * 1024,
            max_bytes_per_sec: None,
            allow_wof_and_dedup: true,
        }
    }
}

/// Why a marked copy failed re-verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyProblem {
    /// The marked copy changed, vanished or cannot be read.
    Copy {
        /// Details.
        reason: SkipReason,
    },
    /// The keeper changed, vanished or cannot be read, so deleting any
    /// copy of this group is unsafe.
    Keeper {
        /// Details.
        reason: SkipReason,
    },
    /// The content no longer matches.
    ContentDiffers,
}

/// A marked copy that must be unmarked before deleting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyFailure {
    /// Group id.
    pub group: u64,
    /// File index in the group.
    pub index: usize,
    /// What went wrong.
    pub problem: VerifyProblem,
}

/// Why verification stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyError {
    /// The selection does not fit the report.
    #[error(transparent)]
    Selection(#[from] SelectionError),
    /// Cancelled; nothing should be deleted on a partial check.
    #[error("verification was cancelled")]
    Cancelled,
}

/// Re-checks every marked copy against its group's keeper.
///
/// Returns the copies that failed; the app unmarks them (or aborts) before
/// handing the queue to `strata-clean`. An empty result means every marked
/// copy is still identical to a keeper that still exists.
///
/// # Errors
///
/// [`VerifyError::Cancelled`] or a selection/report mismatch.
pub fn verify_selection(
    report: &DuplicateReport,
    selection: &Selection,
    cfg: &VerifyConfig,
    cancel: &CancelToken,
) -> Result<Vec<VerifyFailure>, VerifyError> {
    let marked = selection.marked_files(report)?;
    let throttle = Throttle::new(cfg.max_bytes_per_sec);
    let mut buf = vec![0u8; cfg.read_buffer.max(64 * 1024)];
    let mut failures = Vec::new();
    let mut keeper_hash: Option<(u64, Result<(), SkipReason>)> = None;
    for (group, index, copy) in marked {
        if cancel.is_cancelled() {
            return Err(VerifyError::Cancelled);
        }
        let g = report.group(group).ok_or(SelectionError::ReportMismatch)?;
        let sel = selection
            .group(group)
            .ok_or(SelectionError::ReportMismatch)?;
        let keeper = &g.files[sel.keeper()];
        let fail = |problem| VerifyFailure {
            group,
            index,
            problem,
        };
        let expect = |f: &DupFile| Expect {
            file_ref: f.file_ref,
            state: Some((f.size, f.mtime)),
            allow_wof_and_dedup: cfg.allow_wof_and_dedup,
        };
        match cfg.mode {
            VerifyMode::ByteCompare => {
                let k = match hash::open_checked(&keeper.path, Access::Read, &expect(keeper)) {
                    Ok(k) => k,
                    Err(reason) => {
                        failures.push(fail(VerifyProblem::Keeper { reason }));
                        continue;
                    }
                };
                let c = match hash::open_checked(&copy.path, Access::Read, &expect(copy)) {
                    Ok(c) => c,
                    Err(reason) => {
                        failures.push(fail(VerifyProblem::Copy { reason }));
                        continue;
                    }
                };
                match hash::same_bytes((&k.0, &k.1), (&c.0, &c.1), &mut buf, &throttle, cancel) {
                    Ok(true) => {}
                    Ok(false) => failures.push(fail(VerifyProblem::ContentDiffers)),
                    Err(HashFail::Cancelled) => return Err(VerifyError::Cancelled),
                    // Either side may have changed mid-compare; blame the
                    // copy, which is what gets unmarked.
                    Err(HashFail::Skip(reason)) => {
                        failures.push(fail(VerifyProblem::Copy { reason }))
                    }
                }
            }
            VerifyMode::FullHash => {
                if keeper_hash.as_ref().is_none_or(|(id, _)| *id != group) {
                    let r = rehash(keeper, &expect(keeper), g.hash, &mut buf, &throttle, cancel)?;
                    keeper_hash = Some((group, r));
                }
                if let Some((_, Err(reason))) = &keeper_hash {
                    failures.push(fail(VerifyProblem::Keeper {
                        reason: reason.clone(),
                    }));
                    continue;
                }
                if let Err(reason) =
                    rehash(copy, &expect(copy), g.hash, &mut buf, &throttle, cancel)?
                {
                    failures.push(fail(if reason == SkipReason::Changed {
                        VerifyProblem::ContentDiffers
                    } else {
                        VerifyProblem::Copy { reason }
                    }));
                }
            }
        }
    }
    Ok(failures)
}

/// `Ok(Ok(()))` when the file still hashes to `want`; a hash mismatch is
/// reported as [`SkipReason::Changed`].
fn rehash(
    f: &DupFile,
    expect: &Expect,
    want: [u8; 32],
    buf: &mut [u8],
    throttle: &Throttle,
    cancel: &CancelToken,
) -> Result<Result<(), SkipReason>, VerifyError> {
    let (file, facts) = match hash::open_checked(&f.path, Access::Read, expect) {
        Ok(x) => x,
        Err(reason) => return Ok(Err(reason)),
    };
    match hash::full_hash(&file, &facts, buf, throttle, cancel, &|_| {}) {
        Ok(h) if h == want => Ok(Ok(())),
        Ok(_) => Ok(Err(SkipReason::Changed)),
        Err(HashFail::Cancelled) => Err(VerifyError::Cancelled),
        Err(HashFail::Skip(reason)) => Ok(Err(reason)),
    }
}
