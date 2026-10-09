//! Proof of explicit user consent, as types.
//!
//! Actions that affect things outside the cleanup queue (closing another
//! app, emptying the whole Recycle Bin) take a [`Consent`] value. A consent
//! cannot be built by struct literal, `Default` or deserialization; it is
//! minted only by [`Prompt::confirm`], which consumes the [`Prompt`] that
//! describes exactly what the user was shown. It is single-use (not `Clone`),
//! cannot cross threads (not `Send`), and expires.
//!
//! The app's IPC handler for the user's click is the only intended caller of
//! [`Prompt::confirm`]. Nothing in this crate calls it outside tests.

use std::marker::PhantomData;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long a confirmation stays valid.
pub const CONSENT_TTL: Duration = Duration::from_secs(120);

/// An action that needs explicit consent. `describe` is the exact text the
/// UI must show before the user confirms.
pub trait ConsentAction: std::fmt::Debug {
    /// Text shown to the user.
    fn describe(&self) -> String;
}

/// Ask another app to close (WM_CLOSE or Restart Manager shutdown, never a
/// kill).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseApp {
    /// Process id.
    pub pid: u32,
    /// Process start time (FILETIME), so a reused PID cannot be closed by
    /// mistake.
    pub start_time: u64,
    /// Name shown to the user.
    pub app_name: String,
}

impl ConsentAction for CloseApp {
    fn describe(&self) -> String {
        format!(
            "Ask {} (PID {}) to close? It may prompt you to save your work.",
            self.app_name, self.pid
        )
    }
}

/// Empty the Recycle Bin of one drive, or of every drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyRecycleBin {
    /// Drive root such as `C:\`; `None` for every drive.
    pub root: Option<String>,
    /// Items in the bin when the prompt was built.
    pub items: u64,
    /// Bytes in the bin when the prompt was built.
    pub bytes: u64,
}

impl ConsentAction for EmptyRecycleBin {
    fn describe(&self) -> String {
        let scope = self
            .root
            .as_deref()
            .map_or_else(|| "on every drive".to_string(), |r| format!("on {r}"));
        format!(
            "Permanently delete {} items ({} bytes) in the Recycle Bin {scope}? This cannot be undone.",
            self.items, self.bytes
        )
    }
}

/// Schedule a stubborn file for deletion at the next restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteOnReboot {
    /// The file.
    pub path: String,
}

impl ConsentAction for DeleteOnReboot {
    fn describe(&self) -> String {
        format!(
            "Delete {} the next time Windows starts? It cannot be recovered from the Recycle Bin.",
            self.path
        )
    }
}

/// What the UI shows before asking for confirmation.
#[derive(Debug)]
pub struct Prompt<A: ConsentAction> {
    action: A,
}

impl<A: ConsentAction> Prompt<A> {
    /// Wraps an action for display.
    #[must_use]
    pub fn new(action: A) -> Self {
        Self { action }
    }

    /// The text to show.
    #[must_use]
    pub fn text(&self) -> String {
        self.action.describe()
    }

    /// The action being asked about.
    #[must_use]
    pub fn action(&self) -> &A {
        &self.action
    }

    /// Records that the user explicitly confirmed this prompt just now.
    ///
    /// Call only from the handler of the user's confirmation click.
    #[must_use]
    pub fn confirm(self) -> Consent<A> {
        Consent {
            action: self.action,
            granted_at: Instant::now(),
            _not_send: PhantomData,
        }
    }
}

/// Proof the user confirmed `A`. See the module docs.
#[derive(Debug)]
pub struct Consent<A: ConsentAction> {
    action: A,
    granted_at: Instant,
    _not_send: PhantomData<*const ()>,
}

/// Why a consent was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ConsentError {
    /// Older than [`CONSENT_TTL`]; ask again.
    #[error("the confirmation expired; please confirm again")]
    Expired,
    /// The situation changed since the prompt (different process, bin
    /// contents changed); ask again with the new details.
    #[error("things changed since you confirmed; please review and confirm again")]
    Stale,
}

impl<A: ConsentAction> Consent<A> {
    /// The confirmed action.
    #[must_use]
    pub fn action(&self) -> &A {
        &self.action
    }

    /// Validates freshness at `now`, consuming the consent.
    ///
    /// # Errors
    ///
    /// [`ConsentError::Expired`] after [`CONSENT_TTL`].
    pub fn redeem_at(self, now: Instant) -> Result<A, ConsentError> {
        if now.saturating_duration_since(self.granted_at) > CONSENT_TTL {
            return Err(ConsentError::Expired);
        }
        Ok(self.action)
    }

    /// [`Consent::redeem_at`] with the current time.
    ///
    /// # Errors
    ///
    /// See [`Consent::redeem_at`].
    pub fn redeem(self) -> Result<A, ConsentError> {
        self.redeem_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_then_redeem() {
        let p = Prompt::new(CloseApp {
            pid: 1,
            start_time: 2,
            app_name: "x.exe".into(),
        });
        assert!(p.text().contains("x.exe (PID 1)"));
        let c = p.confirm();
        assert_eq!(c.redeem().unwrap().pid, 1);
    }

    #[test]
    fn expired_consent_is_rejected() {
        let c = Prompt::new(EmptyRecycleBin {
            root: None,
            items: 3,
            bytes: 10,
        })
        .confirm();
        let later = Instant::now() + CONSENT_TTL + Duration::from_secs(1);
        assert_eq!(c.redeem_at(later).unwrap_err(), ConsentError::Expired);
    }

    #[test]
    fn empty_bin_text_is_explicit() {
        let t = Prompt::new(EmptyRecycleBin {
            root: Some(r"D:\".into()),
            items: 3,
            bytes: 10,
        })
        .text();
        assert!(t.contains("Permanently delete 3 items"), "{t}");
        assert!(t.contains(r"on D:\"), "{t}");
    }
}
