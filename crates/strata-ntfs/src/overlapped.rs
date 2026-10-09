//! Overlapped reads: several reads in flight on one Windows handle.
//!
//! Synchronous handles serialize every request on the file object, so a
//! scan that wants queue depth on a raw volume needs a second handle opened
//! with `FILE_FLAG_OVERLAPPED` and `ReadFile` calls that return before the
//! data arrives. This module is the crate's only `unsafe` code; everything
//! it exposes is safe:
//!
//! - [`Batch`] owns the destination buffer and every `OVERLAPPED` while
//!   reads are in flight, so neither can be freed or moved under the kernel.
//! - Dropping a [`Batch`] early cancels its reads and waits for them before
//!   releasing memory.
//!
//! Off Windows the types exist but cannot be constructed.

#![allow(unsafe_code)]

/// One read of a [`Batch`]: `len` bytes at device `offset` into the batch
/// buffer at byte `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Part {
    pub offset: u64,
    pub at: usize,
    pub len: usize,
}

#[cfg(windows)]
pub(crate) use imp::{Batch, OverlappedFile};

#[cfg(not(windows))]
pub(crate) use stub::{Batch, OverlappedFile};

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE};
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0,
    };
    use windows::Win32::System::Threading::CreateEventW;
    use windows::core::PCWSTR;

    use super::Part;
    use crate::io::AlignedBuf;

    /// Win32 `FILE_FLAG_OVERLAPPED`.
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    /// `STATUS_PENDING`: the value of `OVERLAPPED::Internal` while a request runs.
    const STATUS_PENDING: usize = 0x103;

    fn to_io(e: &windows::core::Error) -> io::Error {
        io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
    }

    /// A handle opened for overlapped reads. Never read through `std`: its
    /// synchronous read paths do not support overlapped handles.
    #[derive(Debug)]
    pub(crate) struct OverlappedFile {
        file: File,
    }

    impl OverlappedFile {
        /// Opens `path` for reading with read/write sharing, `flags` and
        /// `FILE_FLAG_OVERLAPPED`.
        pub(crate) fn open(path: &Path, share: u32, flags: u32) -> io::Result<Self> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(share)
                .custom_flags(flags | FILE_FLAG_OVERLAPPED)
                .open(path)?;
            Ok(Self { file })
        }

        fn handle(&self) -> HANDLE {
            HANDLE(self.file.as_raw_handle())
        }

        /// Issues every part as its own overlapped read into `buf`. A part
        /// that cannot be issued (out of bounds, too large, or rejected by
        /// the device) stops issuing; [`Batch::wait`] then reports the error.
        pub(crate) fn start(&self, buf: AlignedBuf, parts: &[Part]) -> Batch<'_> {
            let mut batch = Batch {
                file: self,
                pending: Vec::with_capacity(parts.len()),
                error: None,
                buf,
            };
            // NOTE: the base pointer is taken once, after the buffer has
            // moved into the batch, and never re-derived while reads are in
            // flight: each part gets a disjoint slice of it.
            let total = batch.buf.len();
            let base = batch.buf.as_mut_slice().as_mut_ptr();
            for p in parts {
                if p.at.checked_add(p.len).is_none_or(|end| end > total) || p.len == 0 {
                    batch.error = Some(io::ErrorKind::InvalidInput.into());
                    break;
                }
                if u32::try_from(p.len).is_err() {
                    batch.error = Some(io::ErrorKind::InvalidInput.into());
                    break;
                }
                // SAFETY: no security attributes or name; the returned handle
                // is owned by the pending read and closed exactly once in
                // `Batch::drop`.
                let event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
                    Ok(e) => e,
                    Err(e) => {
                        batch.error = Some(to_io(&e));
                        break;
                    }
                };
                let mut ov = Box::new(OVERLAPPED {
                    Anonymous: OVERLAPPED_0 {
                        Anonymous: OVERLAPPED_0_0 {
                            Offset: p.offset as u32,
                            OffsetHigh: (p.offset >> 32) as u32,
                        },
                    },
                    hEvent: event,
                    ..OVERLAPPED::default()
                });
                // SAFETY: `p.at..p.at + p.len` was checked to lie inside the
                // buffer, and parts never overlap one another, so this slice
                // aliases nothing else that is live.
                let dst = unsafe { std::slice::from_raw_parts_mut(base.add(p.at), p.len) };
                let ov_ptr: *mut OVERLAPPED = &mut *ov;
                // SAFETY: the handle is open for overlapped reads; `dst` and
                // `*ov` stay valid and unmoved until the request completes,
                // because the batch owns both (the OVERLAPPED is boxed, the
                // buffer is heap memory) and `Batch::drop` cancels and waits
                // for every request it issued before freeing them.
                let issued = unsafe { ReadFile(self.handle(), Some(dst), None, Some(ov_ptr)) };
                match issued {
                    Ok(()) => {}
                    Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
                    Err(e) => {
                        // SAFETY: the event was created above and is not
                        // referenced by any request.
                        let _ = unsafe { CloseHandle(event) };
                        batch.error = Some(to_io(&e));
                        break;
                    }
                }
                batch.pending.push(Pending {
                    ov,
                    event,
                    len: p.len,
                    done: false,
                });
            }
            batch
        }
    }

    /// One issued read.
    struct Pending {
        ov: Box<OVERLAPPED>,
        event: HANDLE,
        len: usize,
        done: bool,
    }

    /// Reads in flight into one buffer.
    pub(crate) struct Batch<'a> {
        file: &'a OverlappedFile,
        pending: Vec<Pending>,
        error: Option<io::Error>,
        buf: AlignedBuf,
    }

    impl std::fmt::Debug for Batch<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Batch")
                .field("pending", &self.pending.len())
                .field("error", &self.error)
                .finish_non_exhaustive()
        }
    }

    impl Batch<'_> {
        /// Waits for every read and returns the buffer. Fails if any read
        /// failed, came up short, or could not be issued.
        pub(crate) fn wait(mut self) -> (AlignedBuf, io::Result<()>) {
            let mut result = self.error.take().map_or(Ok(()), Err);
            let h = self.file.handle();
            for p in &mut self.pending {
                let mut n = 0u32;
                // SAFETY: `p.ov` is the OVERLAPPED of a request issued on `h`
                // and still owned by this batch; waiting blocks until the
                // kernel is done with it.
                let r = unsafe { GetOverlappedResult(h, &*p.ov, &mut n, true) };
                p.done = p.ov.Internal != STATUS_PENDING;
                let r = match r {
                    Ok(()) if n as usize == p.len => Ok(()),
                    Ok(()) => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "short read past the end of the device",
                    )),
                    Err(e) => Err(to_io(&e)),
                };
                if result.is_ok() {
                    result = r;
                }
            }
            if self.pending.iter().any(|p| !p.done) {
                // The batch is dropped below, which leaks the buffer rather
                // than free memory the kernel may still write.
                return (
                    AlignedBuf::new(0, 1),
                    Err(io::Error::other("read did not complete")),
                );
            }
            let buf = std::mem::replace(&mut self.buf, AlignedBuf::new(0, 1));
            (buf, result)
        }
    }

    impl Drop for Batch<'_> {
        fn drop(&mut self) {
            let h = self.file.handle();
            let mut leak = false;
            for p in self.pending.drain(..) {
                let mut p = p;
                if !p.done {
                    let mut n = 0u32;
                    // SAFETY: the request was issued on `h` with this
                    // OVERLAPPED, which is still alive; cancelling a request
                    // that already finished is harmless, and the wait blocks
                    // until the kernel releases the OVERLAPPED and buffer.
                    unsafe {
                        let _ = CancelIoEx(h, Some(&*p.ov));
                        let _ = GetOverlappedResult(h, &*p.ov, &mut n, true);
                    }
                    p.done = p.ov.Internal != STATUS_PENDING;
                }
                if p.done {
                    // SAFETY: the event belongs to this finished request and
                    // is closed exactly once.
                    let _ = unsafe { CloseHandle(p.event) };
                } else {
                    // WARNING: the kernel may still use this OVERLAPPED, its
                    // event and the buffer, so all three are leaked.
                    leak = true;
                    Box::leak(p.ov);
                }
            }
            if leak {
                std::mem::forget(std::mem::replace(&mut self.buf, AlignedBuf::new(0, 1)));
            }
        }
    }
}

#[cfg(not(windows))]
mod stub {
    use std::convert::Infallible;
    use std::io;

    use super::Part;
    use crate::io::AlignedBuf;

    /// Overlapped handles exist only on Windows.
    #[derive(Debug)]
    pub(crate) struct OverlappedFile {
        never: Infallible,
    }

    impl OverlappedFile {
        pub(crate) fn open(_path: &std::path::Path, _share: u32, _flags: u32) -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub(crate) fn start(&self, _buf: AlignedBuf, _parts: &[Part]) -> Batch<'_> {
            match self.never {}
        }
    }

    /// Never constructed off Windows.
    #[derive(Debug)]
    pub(crate) struct Batch<'a> {
        never: Infallible,
        _file: std::marker::PhantomData<&'a ()>,
    }

    impl Batch<'_> {
        pub(crate) fn wait(self) -> (AlignedBuf, io::Result<()>) {
            match self.never {}
        }
    }
}
