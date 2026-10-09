//! Directory listing and child opens relative to a directory handle.
//!
//! Recursive deletes never build child paths: each child is opened by name
//! relative to the already-verified parent handle, so renaming or swapping an
//! ancestor mid-delete cannot redirect us elsewhere.

use std::io;
use std::mem::{offset_of, size_of};

use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_NO_RECALL, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows::Win32::Foundation::{
    ERROR_NO_MORE_FILES, HANDLE, OBJECT_ATTRIBUTE_FLAGS, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_BOTH_DIR_INFO, FILE_SHARE_MODE,
    FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandleEx,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::core::PWSTR;

use super::handle::{ACCESS_SYNCHRONIZE, OwnedHandle};

/// One child from a directory listing.
#[derive(Debug, Clone)]
pub(crate) struct DirEntry {
    pub name: Vec<u16>,
    pub attributes: u32,
    pub file_id: u64,
}

/// Lists a directory opened with `FILE_LIST_DIRECTORY`. Skips `.` and `..`.
pub(crate) fn list_dir(dir: &OwnedHandle) -> io::Result<Vec<DirEntry>> {
    // u64 storage keeps the buffer 8-byte aligned as the API requires.
    let mut buf = vec![0u64; 8 * 1024];
    let bytes = buf.len() * size_of::<u64>();
    let mut out = Vec::new();
    let mut class = FileIdBothDirectoryRestartInfo;
    loop {
        // SAFETY: `buf` is valid and aligned for `bytes` bytes.
        let r = unsafe {
            GetFileInformationByHandleEx(dir.raw(), class, buf.as_mut_ptr().cast(), bytes as u32)
        };
        if let Err(e) = r {
            if e.code() == ERROR_NO_MORE_FILES.to_hresult() {
                return Ok(out);
            }
            return Err(e.into());
        }
        class = FileIdBothDirectoryInfo;
        let base = buf.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            let header = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if offset + header > bytes {
                break;
            }
            // SAFETY: `offset + header` is within `buf`; the record header is
            // read unaligned because NextEntryOffset only promises 8-byte
            // alignment of the kernel's own layout.
            let rec: FILE_ID_BOTH_DIR_INFO =
                unsafe { std::ptr::read_unaligned(base.add(offset).cast()) };
            let name_len = rec.FileNameLength as usize / 2;
            if offset + header + name_len * 2 > bytes {
                break;
            }
            let mut name = vec![0u16; name_len];
            // SAFETY: bounds checked above; copying bytes avoids alignment
            // assumptions about the name field.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    base.add(offset + header),
                    name.as_mut_ptr().cast::<u8>(),
                    name_len * 2,
                );
            }
            let is_dot = name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')];
            if !is_dot {
                out.push(DirEntry {
                    name,
                    attributes: rec.FileAttributes,
                    file_id: rec.FileId as u64,
                });
            }
            if rec.NextEntryOffset == 0 {
                break;
            }
            offset += rec.NextEntryOffset as usize;
        }
    }
}

/// Opens `name` inside `parent` without following a reparse point at the
/// child and without recalling cloud content.
pub(crate) fn open_relative(
    parent: &OwnedHandle,
    name: &[u16],
    access: u32,
    share: FILE_SHARE_MODE,
) -> io::Result<OwnedHandle> {
    let byte_len = u16::try_from(name.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
    let us = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: PWSTR(name.as_ptr().cast_mut()),
    };
    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.raw(),
        ObjectName: &raw const us,
        // Exact-case lookup: case-sensitive directories may hold both
        // `A.txt` and `a.txt`, and the listing gave us the exact name.
        Attributes: OBJECT_ATTRIBUTE_FLAGS(0),
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut h = HANDLE::default();
    let mut iosb = IO_STATUS_BLOCK::default();
    // SAFETY: every pointer refers to a live local; `name` outlives the call
    // and NtCreateFile does not write through `Buffer`.
    let status = unsafe {
        NtCreateFile(
            &mut h,
            FILE_ACCESS_RIGHTS(access | ACCESS_SYNCHRONIZE),
            &oa,
            &mut iosb,
            None,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            share,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_NO_RECALL,
            None,
            0,
        )
    };
    if status.is_err() {
        // SAFETY: pure status translation.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: NtCreateFile succeeded, so `h` is a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw(h) })
}
