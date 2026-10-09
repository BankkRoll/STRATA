//! Diagnostic output.
//!
//! The helper usually runs hidden (UAC launch with `SW_HIDE`, or as a
//! service), so these lines are only visible when it is started from a
//! console during development or a manual test. They never carry request
//! payloads (paths, names): only events and error codes.

/// Writes one diagnostic line to stderr.
macro_rules! diag {
    ($($arg:tt)*) => {
        eprintln!("strata-helper: {}", format_args!($($arg)*))
    };
}

pub(crate) use diag;
