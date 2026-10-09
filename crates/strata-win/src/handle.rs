//! RAII owners for kernel handles and system-allocated memory.

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};

/// An owned kernel handle, closed with `CloseHandle` on drop.
///
/// Never holds `INVALID_HANDLE_VALUE` or null: constructors reject both.
#[derive(Debug)]
pub struct OwnedHandle(HANDLE);

// SAFETY: kernel handles are process-wide values; any thread may use or close
// them. Thread-affinity of the *object* (e.g. a window) is not modelled here.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; sharing a handle value between threads is sound, and the
// objects we wrap (files, pipes, processes, tokens, events) are thread-safe.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    /// Takes ownership of `h`, or returns `None` for null/invalid values.
    ///
    /// # Safety
    ///
    /// `h` must be a handle the caller owns and that nothing else will close.
    #[must_use]
    pub unsafe fn from_raw(h: HANDLE) -> Option<Self> {
        (!h.is_invalid() && !h.0.is_null()).then_some(Self(h))
    }

    /// The raw handle, still owned by `self`.
    #[must_use]
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once. A failure here
        // cannot be acted on, so it is ignored.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Memory returned by APIs that must be freed with `LocalFree`
/// (e.g. `ConvertSidToStringSidW`, `ConvertStringSecurityDescriptor...`).
#[derive(Debug)]
pub(crate) struct LocalBox(*mut c_void);

impl LocalBox {
    /// Takes ownership of a `LocalAlloc`ed pointer (null is allowed).
    ///
    /// # Safety
    ///
    /// `p` must be null or a pointer allocated with `LocalAlloc` that the
    /// caller owns.
    pub(crate) unsafe fn from_raw(p: *mut c_void) -> Self {
        Self(p)
    }

    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for LocalBox {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from LocalAlloc and is freed once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}
