//! Transport errors.

use std::time::Duration;

use strata_win::WinError;

use crate::frame::FrameError;
use crate::protocol::HandshakeReject;
use crate::security::TrustError;

/// Anything that can go wrong on the pipe.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IpcError {
    /// The peer is gone (closed its end, crashed, or the pipe broke). The UI
    /// shows the "helper disconnected" banner and offers to reconnect (§4).
    #[error("the other end of the pipe disconnected")]
    Disconnected,
    /// The operation did not finish in time.
    #[error("timed out")]
    Timeout,
    /// The peer sent bytes that are not a valid frame; the connection has
    /// been closed because the stream cannot be resynchronized.
    #[error("malformed frame: {0}")]
    Frame(#[from] FrameError),
    /// The helper refused the handshake.
    #[error("handshake rejected: {0}")]
    Rejected(HandshakeReject),
    /// The peer failed identity verification.
    #[error("peer is not trusted: {0}")]
    Untrusted(TrustError),
    /// The client exceeded its request rate.
    #[error("request {request_id} rate limited; retry after {retry_after:?}")]
    RateLimited {
        /// The refused request.
        request_id: u32,
        /// When a retry may succeed.
        retry_after: Duration,
    },
    /// A client is already connected (server), or the pipe instance is busy
    /// (client).
    #[error("pipe is busy")]
    Busy,
    /// A message arrived that is not valid in this direction or state.
    #[error("unexpected message: {0}")]
    Unexpected(&'static str),
    /// A Win32 call failed.
    #[error(transparent)]
    Win(#[from] WinError),
}

impl IpcError {
    /// Whether the connection is unusable after this error.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Self::RateLimited { .. } | Self::Timeout)
    }
}
