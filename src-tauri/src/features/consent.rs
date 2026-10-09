//! The confirmation round trip that turns a user's click into a
//! `strata_clean::consent::Consent` (SPEC §15.3, §15.6).
//!
//! The backend never mints consent on its own:
//!
//! 1. A `*_prepare` command builds the action **from backend data** (a lock
//!    holder the backend itself found, the Recycle Bin's current contents,
//!    a tool's exact command line), stores it under a fresh 128-bit random
//!    token and returns `{ token, text }`. The UI shows `text` verbatim.
//! 2. The UI's confirm button calls the action command with the token.
//!    [`PendingConsents::take`] removes it (single use) and checks that it
//!    is neither expired ([`CONSENT_TTL`]) nor confirmed faster than a human
//!    could have read it ([`MIN_REVIEW`]).
//! 3. Only that handler calls `Prompt::confirm`, then redeems the consent
//!    immediately on the same thread (a `Consent` is not `Send`).
//!
//! The webview can only ever confirm what the backend described, and only
//! once. What the UI sends besides the token is never part of the action.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
pub use strata_clean::consent::CONSENT_TTL;

use super::error::{ErrorKind, FeatureError};

/// Confirmations arriving sooner than this after the prompt was issued are
/// rejected: no person reads a destructive prompt that fast, a script does.
pub const MIN_REVIEW: Duration = Duration::from_millis(400);

/// What the UI receives from a `*_prepare` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentTicket {
    /// Single-use token to pass back on confirmation.
    pub token: String,
    /// Exact text to show before the user confirms.
    pub text: String,
    /// Milliseconds until the token expires.
    pub expires_in_ms: u64,
}

/// Why a confirmation was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentFailure {
    /// Unknown or already used.
    UnknownToken,
    /// Older than [`CONSENT_TTL`].
    Expired,
    /// Sooner than [`MIN_REVIEW`].
    TooFast,
}

impl ConsentFailure {
    fn message(self) -> &'static str {
        match self {
            Self::UnknownToken => {
                "This confirmation is no longer valid; please review and confirm again."
            }
            Self::Expired => "The confirmation expired; please review and confirm again.",
            Self::TooFast => "Please read the prompt before confirming.",
        }
    }
}

impl From<ConsentFailure> for FeatureError {
    fn from(f: ConsentFailure) -> Self {
        Self::with_detail(ErrorKind::Consent, f.message(), &f)
    }
}

struct Pending<T> {
    value: T,
    issued: Instant,
}

/// Actions waiting for the user's confirmation, keyed by token.
pub struct PendingConsents<T> {
    inner: Mutex<HashMap<String, Pending<T>>>,
    min_review: Duration,
    ttl: Duration,
}

impl<T> std::fmt::Debug for PendingConsents<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.inner.lock().map_or(0, |m| m.len());
        f.debug_struct("PendingConsents")
            .field("pending", &n)
            .finish()
    }
}

impl<T> Default for PendingConsents<T> {
    fn default() -> Self {
        Self::with_timing(MIN_REVIEW, CONSENT_TTL)
    }
}

impl<T> PendingConsents<T> {
    /// A registry with custom timing (tests).
    #[must_use]
    pub fn with_timing(min_review: Duration, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            min_review,
            ttl,
        }
    }

    /// Stores `value` (described to the user by `text`) and returns the
    /// ticket the UI shows.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Internal`] if the OS random source fails.
    pub fn offer(
        &self,
        value: T,
        text: String,
        now: Instant,
    ) -> Result<ConsentTicket, FeatureError> {
        let token = random_token()?;
        let mut map = self.lock();
        let ttl = self.ttl;
        map.retain(|_, p| now.saturating_duration_since(p.issued) <= ttl);
        map.insert(token.clone(), Pending { value, issued: now });
        Ok(ConsentTicket {
            token,
            text,
            expires_in_ms: u64::try_from(self.ttl.as_millis()).unwrap_or(u64::MAX),
        })
    }

    /// Removes and returns the action for `token` if the confirmation is
    /// valid at `now`. A rejected token is consumed too, so it can never be
    /// retried.
    ///
    /// # Errors
    ///
    /// See [`ConsentFailure`].
    pub fn take(&self, token: &str, now: Instant) -> Result<T, ConsentFailure> {
        let p = self
            .lock()
            .remove(token)
            .ok_or(ConsentFailure::UnknownToken)?;
        let age = now.saturating_duration_since(p.issued);
        if age > self.ttl {
            return Err(ConsentFailure::Expired);
        }
        if age < self.min_review {
            return Err(ConsentFailure::TooFast);
        }
        Ok(p.value)
    }

    /// Drops a pending action without confirming it (the user cancelled).
    pub fn cancel(&self, token: &str) {
        self.lock().remove(token);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending<T>>> {
        // A panic while holding the lock cannot leave the map inconsistent
        // (every operation is a single insert/remove), so recover it.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 128 random bits as hex.
///
/// # Errors
///
/// [`ErrorKind::Internal`] if the OS random source fails.
pub fn random_token() -> Result<String, FeatureError> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| FeatureError::internal(format!("no randomness: {e}")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_clean::consent::{CloseApp, Prompt};

    fn registry() -> PendingConsents<Prompt<CloseApp>> {
        PendingConsents::with_timing(Duration::from_millis(400), Duration::from_secs(120))
    }

    fn prompt() -> Prompt<CloseApp> {
        Prompt::new(CloseApp {
            pid: 42,
            start_time: 7,
            app_name: "Example.exe".into(),
        })
    }

    #[test]
    fn round_trip_mints_consent_for_exactly_what_was_shown() {
        let r = registry();
        let t0 = Instant::now();
        let p = prompt();
        let text = p.text();
        let ticket = r.offer(p, text.clone(), t0).unwrap();
        assert_eq!(ticket.text, text);
        assert_eq!(ticket.token.len(), 32);
        let p = r.take(&ticket.token, t0 + Duration::from_secs(2)).unwrap();
        let consent = p.confirm();
        assert_eq!(consent.redeem().unwrap().pid, 42);
        assert_eq!(
            r.take(&ticket.token, t0 + Duration::from_secs(3))
                .unwrap_err(),
            ConsentFailure::UnknownToken,
            "single use"
        );
    }

    #[test]
    fn rejects_unknown_fast_and_expired_confirmations() {
        let r = registry();
        let t0 = Instant::now();
        assert_eq!(
            r.take("nope", t0).unwrap_err(),
            ConsentFailure::UnknownToken
        );

        let fast = r.offer(prompt(), String::new(), t0).unwrap();
        assert_eq!(
            r.take(&fast.token, t0 + Duration::from_millis(50))
                .unwrap_err(),
            ConsentFailure::TooFast
        );
        assert_eq!(
            r.take(&fast.token, t0 + Duration::from_secs(1))
                .unwrap_err(),
            ConsentFailure::UnknownToken,
            "a rejected token is burned"
        );

        let slow = r.offer(prompt(), String::new(), t0).unwrap();
        assert_eq!(
            r.take(&slow.token, t0 + Duration::from_secs(121))
                .unwrap_err(),
            ConsentFailure::Expired
        );
    }

    #[test]
    fn cancel_and_expiry_prune() {
        let r = registry();
        let t0 = Instant::now();
        let a = r.offer(prompt(), String::new(), t0).unwrap();
        r.cancel(&a.token);
        assert_eq!(
            r.take(&a.token, t0 + Duration::from_secs(1)).unwrap_err(),
            ConsentFailure::UnknownToken
        );
        let b = r.offer(prompt(), String::new(), t0).unwrap();
        let _c = r
            .offer(prompt(), String::new(), t0 + Duration::from_secs(200))
            .unwrap();
        assert_eq!(
            r.take(&b.token, t0 + Duration::from_secs(201)).unwrap_err(),
            ConsentFailure::UnknownToken,
            "pruned when the next prompt was offered"
        );
    }

    #[test]
    fn tokens_are_unique() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
