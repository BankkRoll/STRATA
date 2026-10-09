//! Thin Windows FFI layer: the only module in this crate that uses `unsafe`.
//!
//! Responsibilities:
//! - Opening directories and files through `NtCreateFile`, either by
//!   absolute NT path or relative to an already-open directory handle.
//!   Every open asks for attribute (or list) access only, with
//!   `FILE_OPEN_REPARSE_POINT` (never follow links),
//!   `FILE_OPEN_FOR_BACKUP_INTENT` (open directories) and, for files,
//!   `FILE_OPEN_NO_RECALL` (never hydrate cloud placeholders).
//! - Directory listing via `GetFileInformationByHandleEx` and
//!   `FindFirstFileExW`.
//! - Per-handle queries (standard, basic, id, compression, streams, reparse).
//! - Volume facts and the thread handle used by `CancelSynchronousIo`.
//!
//! Handles are `std::os::windows::io::OwnedHandle` (closed on drop); find
//! handles use [`FindGuard`]. Every function returns `io::Error` with the
//! Win32 error code so callers can classify it.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use strata_core::{FileTime, Times, win32};
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_NO_RECALL,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_OPTIONS,
    NtCreateFile,
};
use windows::Win32::Foundation::{
    HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_COMPRESSION_INFO,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FIND_FIRST_EX_LARGE_FETCH, FileAttributeTagInfo, FileBasicInfo, FileCompressionInfo,
    FileFullDirectoryInfo, FileIdBothDirectoryInfo, FileIdExtdDirectoryInfo, FileIdInfo,
    FileStandardInfo, FileStreamInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch,
    FindFirstFileExW, FindNextFileW, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW,
    GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumePathNameW, SYNCHRONIZE,
    WIN32_FIND_DATAW,
};
use windows::Win32::System::IO::{CancelSynchronousIo, DeviceIoControl, IO_STATUS_BLOCK};
use windows::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
use windows::core::PCWSTR;

use crate::CancelToken;
use crate::parse::{DirInfoClass, RawEntry};

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_INVALID_PARAMETER: i32 = 87;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_NO_MORE_FILES: i32 = 18;
const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_FILENAME_EXCED_RANGE: i32 = 206;
const ERROR_INVALID_FUNCTION: i32 = 1;
const DRIVE_REMOTE: u32 = 4;

/// Converts a `windows` crate error (an HRESULT) back into the Win32 code.
fn to_io(e: &windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::from_raw_os_error(hr as i32)
    }
}

fn raw(h: &OwnedHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

fn nul_terminated(s: &[u16]) -> Vec<u16> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s);
    v.push(0);
    v
}

/// An 8-byte aligned scratch buffer. Directory and stream information
/// records contain `LARGE_INTEGER` fields, and the kernel rejects buffers
/// that are not suitably aligned.
#[derive(Debug)]
pub(crate) struct AlignedBuf(Vec<u64>);

impl AlignedBuf {
    /// A zeroed buffer of at least `bytes` bytes.
    pub(crate) fn new(bytes: usize) -> Self {
        Self(vec![0; bytes.div_ceil(8)])
    }

    fn len_bytes(&self) -> usize {
        self.0.len() * 8
    }

    fn grow(&mut self) {
        let n = self.0.len() * 2;
        self.0.resize(n, 0);
    }

    fn as_mut_ptr(&mut self) -> *mut core::ffi::c_void {
        self.0.as_mut_ptr().cast()
    }

    /// The first `len` bytes.
    pub(crate) fn bytes(&self, len: usize) -> &[u8] {
        let len = len.min(self.len_bytes());
        // SAFETY: the Vec<u64> owns at least `len_bytes()` initialised bytes,
        // `len` is clamped to that, and u8 has no alignment requirement.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast::<u8>(), len) }
    }
}

/// What an [`nt_open`] handle will be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// Directory listing (`FILE_LIST_DIRECTORY`); fails with
    /// `ERROR_DIRECTORY` if the target is not a directory.
    ListDir,
    /// Attribute and metadata queries only.
    Attributes,
}

/// Opens `name` with `NtCreateFile`.
///
/// With `dir`, `name` is a single component relative to that directory,
/// which skips re-parsing the full path and re-checking traverse access on
/// every ancestor. Without `dir`, `name` must be an absolute NT path
/// (`\??\C:\...`). `follow` resolves a reparse point instead of opening it;
/// only the walk root uses it.
pub(crate) fn nt_open(
    dir: Option<&OwnedHandle>,
    name: &[u16],
    mode: OpenMode,
    follow: bool,
) -> io::Result<OwnedHandle> {
    let byte_len = name
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| io::Error::from_raw_os_error(ERROR_FILENAME_EXCED_RANGE))?;
    let us = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: windows::core::PWSTR(name.as_ptr().cast_mut()),
    };
    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: dir.map(raw).unwrap_or_default(),
        ObjectName: &us,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..Default::default()
    };
    let (access, mut options) = match mode {
        OpenMode::ListDir => (
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE,
        ),
        OpenMode::Attributes => (
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            NTCREATEFILE_CREATE_OPTIONS(0),
        ),
    };
    options |= FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_FOR_BACKUP_INTENT;
    // NOTE: NtCreateFile rejects FILE_OPEN_NO_RECALL together with
    // FILE_DIRECTORY_FILE (STATUS_INVALID_PARAMETER). Directory opens do not
    // need it: listing never reads file data, and unpopulated cloud
    // directories are not descended into at all.
    if mode == OpenMode::Attributes {
        options |= FILE_OPEN_NO_RECALL;
    }
    if !follow {
        options |= FILE_OPEN_REPARSE_POINT;
    }
    let mut handle = HANDLE::default();
    let mut iosb = IO_STATUS_BLOCK::default();
    // SAFETY: `us` points into `name`, which outlives the call; `oa`, `iosb`
    // and `handle` are valid for writes for the duration of the call; no EA
    // buffer is passed.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_ACCESS_RIGHTS(access.0),
            &oa,
            &mut iosb,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            options,
            None,
            0,
        )
    };
    if status.is_err() {
        // SAFETY: pure conversion function with no pointer arguments.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: NtCreateFile succeeded, so `handle` is a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

/// Result of one directory-information query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirChunk {
    /// `len` bytes of records were written.
    Data(usize),
    /// The listing is complete.
    End,
    /// The filesystem does not support this information class.
    Unsupported,
}

/// Fetches the next buffer of directory records from `h`.
pub(crate) fn query_dir(
    h: &OwnedHandle,
    class: DirInfoClass,
    buf: &mut AlignedBuf,
) -> io::Result<DirChunk> {
    let cls: FILE_INFO_BY_HANDLE_CLASS = match class {
        DirInfoClass::IdExtd => FileIdExtdDirectoryInfo,
        DirInfoClass::IdBoth => FileIdBothDirectoryInfo,
        DirInfoClass::Full => FileFullDirectoryInfo,
    };
    let len = buf.len_bytes();
    // SAFETY: the buffer is writable for `len` bytes and 8-byte aligned.
    let r = unsafe { GetFileInformationByHandleEx(raw(h), cls, buf.as_mut_ptr(), len as u32) };
    match r {
        // NOTE: the API reports no byte count; records are self-delimiting
        // through NextEntryOffset, so hand the whole buffer to the parser.
        Ok(()) => Ok(DirChunk::Data(len)),
        Err(e) => {
            let err = to_io(&e);
            match err.raw_os_error() {
                Some(ERROR_NO_MORE_FILES) => Ok(DirChunk::End),
                Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION) => {
                    Ok(DirChunk::Unsupported)
                }
                _ => Err(err),
            }
        }
    }
}

/// Closes a `FindFirstFileExW` handle on drop.
struct FindGuard(HANDLE);

impl Drop for FindGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful FindFirstFileExW and is
        // closed exactly once.
        let _ = unsafe { FindClose(self.0) };
    }
}

fn filetime(ft: windows::Win32::Foundation::FILETIME) -> FileTime {
    FileTime((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime))
}

fn entry_from_find(d: &WIN32_FIND_DATAW) -> Option<RawEntry> {
    let len = d
        .cFileName
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(d.cFileName.len());
    let name = d.cFileName[..len].to_vec();
    let dot = u16::from(b'.');
    if name.is_empty() || name == [dot] || name == [dot, dot] {
        return None;
    }
    let attributes = d.dwFileAttributes;
    Some(RawEntry {
        name,
        attributes,
        reparse_tag: if attributes & win32::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            d.dwReserved0
        } else {
            0
        },
        times: Times {
            created: filetime(d.ftCreationTime),
            accessed: filetime(d.ftLastAccessTime),
            modified: filetime(d.ftLastWriteTime),
            changed: FileTime(0),
        },
        logical: (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow),
        allocated: None,
        file_id: None,
    })
}

/// Lists `dir` (an extended `\\?\` path) with `FindFirstFileExW`.
///
/// Returns `Ok(true)` when the listing completed, `Ok(false)` when `cancel`
/// interrupted it (entries read so far are kept).
pub(crate) fn find_list(
    dir: &[u16],
    cancel: &CancelToken,
    out: &mut Vec<RawEntry>,
) -> io::Result<bool> {
    let mut pattern = crate::path::join(dir, &[u16::from(b'*')]);
    pattern.push(0);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated; `data` is a valid out buffer for
    // the FindExInfoBasic level.
    let first = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&raw mut data).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let handle = match first {
        Ok(h) => FindGuard(h),
        Err(e) => {
            let err = to_io(&e);
            // NOTE: a drive root has no `.`/`..` entries, so an empty root
            // reports FILE_NOT_FOUND rather than an empty listing.
            return if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) {
                Ok(true)
            } else {
                Err(err)
            };
        }
    };
    let mut n = 0usize;
    loop {
        if let Some(e) = entry_from_find(&data) {
            out.push(e);
        }
        n += 1;
        if n.is_multiple_of(256) && cancel.is_cancelled() {
            return Ok(false);
        }
        // SAFETY: `handle.0` is a live find handle and `data` a valid buffer.
        if let Err(e) = unsafe { FindNextFileW(handle.0, &mut data) } {
            let err = to_io(&e);
            return if err.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                Ok(true)
            } else {
                Err(err)
            };
        }
    }
}

/// Reads a fixed-size information class into `T`.
///
/// # Safety
///
/// `T` must be the plain-data structure documented for `class`.
unsafe fn query_fixed<T: Default>(
    h: &OwnedHandle,
    class: FILE_INFO_BY_HANDLE_CLASS,
) -> io::Result<T> {
    let mut v = T::default();
    // SAFETY: `v` is valid for writes of size_of::<T>() bytes and, per the
    // caller's contract, has the layout the class expects.
    unsafe {
        GetFileInformationByHandleEx(raw(h), class, (&raw mut v).cast(), size_of::<T>() as u32)
    }
    .map_err(|e| to_io(&e))?;
    Ok(v)
}

/// `FILE_STANDARD_INFO` essentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StandardInfo {
    /// Allocation of the unnamed stream (or the directory index).
    pub allocation: u64,
    /// End of file of the unnamed stream.
    pub end_of_file: u64,
    /// Hardlink count.
    pub links: u32,
}

/// Queries `FileStandardInfo`.
pub(crate) fn standard_info(h: &OwnedHandle) -> io::Result<StandardInfo> {
    // SAFETY: FILE_STANDARD_INFO is the documented layout for FileStandardInfo.
    let s: FILE_STANDARD_INFO = unsafe { query_fixed(h, FileStandardInfo) }?;
    Ok(StandardInfo {
        allocation: s.AllocationSize.max(0) as u64,
        end_of_file: s.EndOfFile.max(0) as u64,
        links: s.NumberOfLinks,
    })
}

/// Queries `FileBasicInfo`: timestamps and attributes.
pub(crate) fn basic_info(h: &OwnedHandle) -> io::Result<(Times, u32)> {
    // SAFETY: FILE_BASIC_INFO is the documented layout for FileBasicInfo.
    let b: FILE_BASIC_INFO = unsafe { query_fixed(h, FileBasicInfo) }?;
    let t = |v: i64| FileTime(v.max(0) as u64);
    Ok((
        Times {
            created: t(b.CreationTime),
            accessed: t(b.LastAccessTime),
            modified: t(b.LastWriteTime),
            changed: t(b.ChangeTime),
        },
        b.FileAttributes,
    ))
}

/// Queries `FileAttributeTagInfo`: attributes and reparse tag.
pub(crate) fn attribute_tag(h: &OwnedHandle) -> io::Result<(u32, u32)> {
    // SAFETY: FILE_ATTRIBUTE_TAG_INFO is the documented layout.
    let a: FILE_ATTRIBUTE_TAG_INFO = unsafe { query_fixed(h, FileAttributeTagInfo) }?;
    Ok((a.FileAttributes, a.ReparseTag))
}

/// Queries `FileIdInfo`: the 128-bit file id.
pub(crate) fn file_id(h: &OwnedHandle) -> io::Result<u128> {
    // SAFETY: FILE_ID_INFO is the documented layout for FileIdInfo.
    let i: FILE_ID_INFO = unsafe { query_fixed(h, FileIdInfo) }?;
    Ok(u128::from_le_bytes(i.FileId.Identifier))
}

/// Queries `FileCompressionInfo`: bytes actually allocated for a compressed
/// or sparse stream (what `GetCompressedFileSizeW` reports), from the handle
/// we already hold instead of reopening by path.
pub(crate) fn compressed_size(h: &OwnedHandle) -> io::Result<u64> {
    // SAFETY: FILE_COMPRESSION_INFO is the documented layout.
    let c: FILE_COMPRESSION_INFO = unsafe { query_fixed(h, FileCompressionInfo) }?;
    Ok(c.CompressedFileSize.max(0) as u64)
}

/// Queries `FileStreamInfo` into `buf`, growing it as needed. Returns the
/// valid bytes (empty when the object has no data streams).
pub(crate) fn stream_info<'b>(h: &OwnedHandle, buf: &'b mut AlignedBuf) -> io::Result<&'b [u8]> {
    const MAX: usize = 16 << 20;
    loop {
        let len = buf.len_bytes();
        // SAFETY: the buffer is writable for `len` bytes and 8-byte aligned.
        let r = unsafe {
            GetFileInformationByHandleEx(raw(h), FileStreamInfo, buf.as_mut_ptr(), len as u32)
        };
        match r {
            Ok(()) => return Ok(buf.bytes(len)),
            Err(e) => {
                let err = to_io(&e);
                match err.raw_os_error() {
                    Some(ERROR_HANDLE_EOF) => return Ok(&[]),
                    Some(ERROR_MORE_DATA) if len < MAX => buf.grow(),
                    _ => return Err(err),
                }
            }
        }
    }
}

/// Reads the raw `REPARSE_DATA_BUFFER` of `h` into `buf`.
pub(crate) fn reparse_data<'b>(h: &OwnedHandle, buf: &'b mut AlignedBuf) -> io::Result<&'b [u8]> {
    let len = buf.len_bytes();
    let mut returned = 0u32;
    // SAFETY: `buf` is writable for `len` bytes; the handle is synchronous so
    // no OVERLAPPED is needed; `returned` is a valid out pointer.
    unsafe {
        DeviceIoControl(
            raw(h),
            FSCTL_GET_REPARSE_POINT,
            None,
            0,
            Some(buf.as_mut_ptr()),
            len as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| to_io(&e))?;
    Ok(buf.bytes(returned as usize))
}

/// Facts about the volume holding a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolumeFacts {
    /// Mount path as returned by `GetVolumePathNameW` (verbatim prefix kept).
    pub mount: Vec<u16>,
    pub filesystem: String,
    pub cluster_size: u64,
    pub total: u64,
    pub free: u64,
    pub is_remote: bool,
}

/// Reads volume facts for an extended path.
pub(crate) fn volume_facts(ext_path: &[u16]) -> io::Result<VolumeFacts> {
    let p = nul_terminated(ext_path);
    let mut mount = vec![0u16; ext_path.len() + 2];
    // SAFETY: `p` is NUL-terminated; `mount` is a writable buffer at least as
    // long as the input path plus a separator and NUL.
    unsafe { GetVolumePathNameW(PCWSTR(p.as_ptr()), &mut mount) }.map_err(|e| to_io(&e))?;
    let mlen = mount.iter().position(|&u| u == 0).unwrap_or(mount.len());
    mount.truncate(mlen);
    let m = nul_terminated(&mount);
    let (mut spc, mut bps) = (0u32, 0u32);
    // SAFETY: `m` is NUL-terminated; out pointers are valid locals.
    unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(m.as_ptr()),
            Some(&mut spc),
            Some(&mut bps),
            None,
            None,
        )
    }
    .map_err(|e| to_io(&e))?;
    let (mut total, mut free) = (0u64, 0u64);
    // SAFETY: as above.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(m.as_ptr()), None, Some(&mut total), Some(&mut free)) }
        .map_err(|e| to_io(&e))?;
    let mut fs = [0u16; 64];
    // SAFETY: `m` is NUL-terminated; `fs` is a writable buffer.
    let fs_ok =
        unsafe { GetVolumeInformationW(PCWSTR(m.as_ptr()), None, None, None, None, Some(&mut fs)) }
            .is_ok();
    let fs_len = fs.iter().position(|&u| u == 0).unwrap_or(fs.len());
    // SAFETY: `m` is NUL-terminated.
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(m.as_ptr())) };
    Ok(VolumeFacts {
        is_remote: drive_type == DRIVE_REMOTE || crate::path::is_unc(&mount),
        mount,
        filesystem: if fs_ok {
            String::from_utf16_lossy(&fs[..fs_len])
        } else {
            String::new()
        },
        cluster_size: u64::from(spc) * u64::from(bps),
        total,
        free,
    })
}

/// A handle to a thread that can have its synchronous I/O cancelled.
#[derive(Debug)]
pub(crate) struct ThreadHandle(OwnedHandle);

impl ThreadHandle {
    /// Opens the calling thread with `THREAD_TERMINATE`, the access
    /// `CancelSynchronousIo` requires.
    pub(crate) fn current() -> io::Result<Self> {
        // SAFETY: no pointer arguments; the returned handle is owned by us.
        let h = unsafe { OpenThread(THREAD_TERMINATE, false, GetCurrentThreadId()) }
            .map_err(|e| to_io(&e))?;
        // SAFETY: OpenThread succeeded, so `h` is a fresh handle we own.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(h.0) }))
    }

    /// Cancels the thread's pending synchronous I/O, if any. Returns whether
    /// something was cancelled.
    pub(crate) fn cancel_sync_io(&self) -> bool {
        // SAFETY: the handle is a live thread handle with THREAD_TERMINATE.
        unsafe { CancelSynchronousIo(raw(&self.0)) }.is_ok()
    }
}

/// Test-only helpers that build fixture trees needing FFI (sparse files,
/// NTFS compression).
#[cfg(test)]
pub(crate) mod fixture {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_SET_COMPRESSION, FSCTL_SET_SPARSE};

    fn ioctl(f: &File, code: u32, input: &[u8]) -> io::Result<()> {
        let mut returned = 0u32;
        // SAFETY: `input` is readable for its length; no output buffer; the
        // std File handle is synchronous.
        unsafe {
            DeviceIoControl(
                HANDLE(f.as_raw_handle()),
                code,
                Some(input.as_ptr().cast()),
                input.len() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| super::to_io(&e))
    }

    /// Marks an open (writable) file sparse.
    pub(crate) fn set_sparse(f: &File) -> io::Result<()> {
        ioctl(f, FSCTL_SET_SPARSE, &[1, 0, 0, 0])
    }

    /// Enables LZNT1 compression on an open (writable) file.
    pub(crate) fn set_compressed(f: &File) -> io::Result<()> {
        // COMPRESSION_FORMAT_DEFAULT
        ioctl(f, FSCTL_SET_COMPRESSION, &1u16.to_le_bytes())
    }

    /// A synchronous anonymous pipe `(read, write)`. Unlike std's child
    /// pipes (overlapped I/O under the hood), a read on this blocks in the
    /// kernel and can be aborted by `CancelSynchronousIo`.
    pub(crate) fn sync_pipe() -> io::Result<(File, File)> {
        let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
        // SAFETY: both out pointers are valid locals; default attributes.
        unsafe { windows::Win32::System::Pipes::CreatePipe(&mut r, &mut w, None, 0) }
            .map_err(|e| super::to_io(&e))?;
        // SAFETY: CreatePipe succeeded; each handle is owned exactly once.
        Ok(unsafe { (File::from_raw_handle(r.0), File::from_raw_handle(w.0)) })
    }
}
