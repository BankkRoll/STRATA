//! What the scan saw, re-verified against the handle right before acting.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime};

use crate::error::Change;
use crate::guard::ItemFacts;

/// Identity and state the scan recorded for an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    /// File reference from the scan.
    pub file_ref: FileRef,
    /// Whether the scan saw a directory.
    pub is_dir: bool,
    /// Logical size from the scan. Verified for files only; for directories
    /// it is the subtree total, used for capacity and threshold checks.
    pub size: u64,
    /// Last-write time from the scan. Verified for files only.
    pub modified: FileTime,
}

impl Expected {
    /// Compares against facts read from a handle.
    ///
    /// # Errors
    ///
    /// The first [`Change`] found.
    pub fn verify(&self, facts: &ItemFacts) -> Result<(), Change> {
        if self.file_ref.is_synthetic() {
            return Err(Change::SyntheticReference);
        }
        if !facts.identity.matches(self.file_ref) {
            return Err(Change::Identity {
                expected: self.file_ref,
                found: facts.identity.file_id,
            });
        }
        if facts.is_dir() != self.is_dir {
            return Err(Change::Kind {
                expected_dir: self.is_dir,
            });
        }
        if !self.is_dir {
            if facts.size != self.size {
                return Err(Change::Size {
                    expected: self.size,
                    found: facts.size,
                });
            }
            if facts.modified != self.modified {
                return Err(Change::Modified {
                    expected: self.modified,
                    found: facts.modified,
                });
            }
        }
        Ok(())
    }

    /// Expectation matching `facts` exactly (for callers that just checked
    /// the item, such as tests and retry flows after a fresh scan).
    #[must_use]
    pub fn from_facts(facts: &ItemFacts) -> Self {
        Self {
            file_ref: FileRef(facts.identity.file_index),
            is_dir: facts.is_dir(),
            size: facts.size,
            modified: facts.modified,
        }
    }
}

/// Cooperative cancellation shared between the UI and a running cleanup.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Items already running finish; no new item
    /// starts.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::FileIdentity;

    fn facts(id: u128, dir: bool, size: u64, mtime: u64) -> ItemFacts {
        ItemFacts {
            identity: FileIdentity {
                volume_serial: 1,
                file_id: id,
                file_index: id as u64,
            },
            attributes: if dir { 0x10 } else { 0x20 },
            reparse_tag: 0,
            size,
            modified: FileTime(mtime),
            links: 1,
        }
    }

    #[test]
    fn verify_detects_each_change() {
        let e = Expected {
            file_ref: FileRef(42),
            is_dir: false,
            size: 10,
            modified: FileTime(5),
        };
        assert!(e.verify(&facts(42, false, 10, 5)).is_ok());
        assert!(matches!(
            e.verify(&facts(43, false, 10, 5)),
            Err(Change::Identity { .. })
        ));
        assert!(matches!(
            e.verify(&facts(42, true, 10, 5)),
            Err(Change::Kind { .. })
        ));
        assert!(matches!(
            e.verify(&facts(42, false, 11, 5)),
            Err(Change::Size { .. })
        ));
        assert!(matches!(
            e.verify(&facts(42, false, 10, 6)),
            Err(Change::Modified { .. })
        ));
        let d = Expected { is_dir: true, ..e };
        assert!(d.verify(&facts(42, true, 999, 999)).is_ok());
        let synthetic = Expected {
            file_ref: FileRef(FileRef::SYNTHETIC_BIT | 42),
            ..e
        };
        assert_eq!(
            synthetic.verify(&facts(42, false, 10, 5)),
            Err(Change::SyntheticReference)
        );
    }

    #[test]
    fn refs_128_bit_ids_fall_back_to_file_index() {
        let mut f = facts(0, false, 0, 0);
        f.identity.file_id = u128::MAX - 5;
        f.identity.file_index = 77;
        assert!(f.identity.matches(FileRef(77)));
        assert!(!f.identity.matches(FileRef(78)));
    }

    #[test]
    fn cancel_token_is_shared() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }
}
