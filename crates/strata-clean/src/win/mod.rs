//! Thin Win32 layer. Every `unsafe` block in the crate lives under here, each
//! with a `// SAFETY:` comment, behind RAII wrappers for handles, COM
//! apartments and Restart Manager sessions.

pub(crate) mod fileops;
pub(crate) mod handle;
pub(crate) mod ntdir;
pub(crate) mod process;
pub(crate) mod rm;
pub(crate) mod shell;
pub(crate) mod vol;

/// NUL-terminated UTF-16 copy of `s`.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// NUL-terminated UTF-16 copy of an OS string.
pub(crate) fn wide_os(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}
