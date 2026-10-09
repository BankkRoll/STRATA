use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation flag shared between the caller and a walk.
///
/// Cloning is cheap; every clone observes the same flag. The walker checks it
/// between directories, between listing buffers of large directories, and
/// between files of the allocation pass, so cancellation takes effect within
/// one buffer's worth of work per thread.
///
/// # Example
///
/// ```
/// use strata_walk::CancelToken;
/// let token = CancelToken::new();
/// let remote = token.clone();
/// remote.cancel();
/// assert!(token.is_cancelled());
/// ```
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Creates a token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
