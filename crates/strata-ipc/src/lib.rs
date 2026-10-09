//! The helper ↔ app channel.
//!
//! The unelevated app talks to the elevated `strata-helper` over a named
//! pipe. This crate defines everything on that pipe:
//!
//! - [`protocol`]: versioned messages (handshake, requests, responses and
//!   streamed scan events), each tagged with a request id.
//! - [`frame`]: the length-prefixed binary framing with postcard payloads,
//!   strictly bounds-checked for hostile input.
//! - [`pipe`]: the overlapped named-pipe transport ([`pipe::PipeServer`] for
//!   the helper, [`pipe::PipeClient`] for the app), with timeouts and typed
//!   disconnect handling.
//! - [`security`]: the pipe DACL and integrity label, unguessable session
//!   pipe names, and peer verification (image path + Authenticode signer).
//! - [`rate`]: the per-connection token bucket.
//!
//! The helper binary wires these to the scanners; this crate holds no scan
//! logic.

#![cfg(windows)]

mod error;
pub mod frame;
pub mod pipe;
pub mod protocol;
pub mod rate;
pub mod security;

pub use error::IpcError;
