//! A recursive folder watcher over `ReadDirectoryChangesW`, for live
//! updates of volumes the walker scanned ([`strata_live::SubtreeWatcher`]).
//!
//! The directory handle is opened for overlapped I/O so a wait can time out
//! and the watcher can stop; dropping it cancels the pending read and waits
//! for the kernel to release the buffer before freeing it.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use strata_core::WideName;
use strata_live::{SourceError, SubtreeWatcher, WatchBatch, WatchChange, WatchKind};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

/// Notification buffer size; larger bursts report an overflow.
const BUF_LEN: usize = 64 * 1024;

/// `FILE_NOTIFY_INFORMATION` header size (three `u32`s) before the name.
const HEADER: usize = 12;

/// Watches one folder and everything below it.
pub struct DirWatcher {
    dir: HANDLE,
    event: HANDLE,
    // NOTE: boxed so their addresses stay fixed while the kernel writes into
    // them; both outlive any pending read (see `Drop`).
    overlapped: Box<OVERLAPPED>,
    buf: Box<[u32]>,
    pending: bool,
}

impl std::fmt::Debug for DirWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirWatcher")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

// SAFETY: the handles and buffers are owned by this value and only used
// through `&mut self`; moving it to another thread moves that ownership.
unsafe impl Send for DirWatcher {}

impl DirWatcher {
    /// Starts watching `dir` recursively.
    ///
    /// # Errors
    ///
    /// The folder cannot be opened.
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(std::io::Error::from)?;
        // SAFETY: plain event creation (manual reset, unsignaled, unnamed).
        let event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
            Ok(e) => e,
            Err(e) => {
                // SAFETY: `handle` is ours and not used again.
                let _ = unsafe { CloseHandle(handle) };
                return Err(e.into());
            }
        };
        let overlapped = Box::new(OVERLAPPED {
            hEvent: event,
            ..OVERLAPPED::default()
        });
        Ok(Self {
            dir: handle,
            event,
            overlapped,
            buf: vec![0u32; BUF_LEN / 4].into_boxed_slice(),
            pending: false,
        })
    }

    fn arm(&mut self) -> Result<(), SourceError> {
        // SAFETY: the buffer and OVERLAPPED are heap allocations owned by
        // `self` that stay alive until the read completes or is cancelled
        // and reaped in `Drop`.
        unsafe {
            ReadDirectoryChangesW(
                self.dir,
                self.buf.as_mut_ptr().cast(),
                u32::try_from(BUF_LEN).unwrap_or(u32::MAX),
                true,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES,
                None,
                Some(&raw mut *self.overlapped),
                None,
            )
        }
        .map_err(|e| SourceError::Io(e.to_string()))?;
        self.pending = true;
        Ok(())
    }
}

/// Parses a `FILE_NOTIFY_INFORMATION` chain of `len` bytes.
#[must_use]
pub fn parse(bytes: &[u8]) -> Vec<WatchChange> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let u32_at = |i: usize| {
        bytes
            .get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    while let (Some(next), Some(action), Some(name_len)) =
        (u32_at(at), u32_at(at + 4), u32_at(at + 8))
    {
        let start = at + HEADER;
        let Some(name) = bytes.get(start..start + name_len as usize) else {
            break;
        };
        let units: Vec<u16> = name
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let kind = match action {
            a if a == FILE_ACTION_ADDED.0 => Some(WatchKind::Added),
            a if a == FILE_ACTION_REMOVED.0 => Some(WatchKind::Removed),
            a if a == FILE_ACTION_MODIFIED.0 => Some(WatchKind::Modified),
            a if a == FILE_ACTION_RENAMED_OLD_NAME.0 => Some(WatchKind::RenamedFrom),
            a if a == FILE_ACTION_RENAMED_NEW_NAME.0 => Some(WatchKind::RenamedTo),
            _ => None,
        };
        if let Some(kind) = kind {
            let path = units
                .split(|&u| u == u16::from(b'\\'))
                .filter(|c| !c.is_empty())
                .map(|c| WideName::from_units(c.to_vec()))
                .collect();
            out.push(WatchChange { kind, path });
        }
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    out
}

impl SubtreeWatcher for DirWatcher {
    fn next(&mut self, wait: Option<Duration>) -> Result<WatchBatch, SourceError> {
        if !self.pending {
            self.arm()?;
        }
        let ms = wait.map_or(u32::MAX, |d| {
            u32::try_from(d.as_millis()).unwrap_or(u32::MAX - 1)
        });
        // SAFETY: `event` is a live handle owned by `self`.
        let r = unsafe { WaitForSingleObject(self.event, ms) };
        if r == WAIT_TIMEOUT {
            return Ok(WatchBatch::Changes(Vec::new()));
        }
        if r != WAIT_OBJECT_0 {
            return Err(SourceError::Io("waiting for folder changes failed".into()));
        }
        let mut n = 0u32;
        // SAFETY: the read signalled completion; the OVERLAPPED belongs to it.
        let done = unsafe {
            GetOverlappedResult(self.dir, &raw const *self.overlapped, &raw mut n, false)
        };
        self.pending = false;
        // NOTE: a failed read means the folder was deleted, renamed away or
        // its volume dismounted; the watch cannot continue either way.
        if done.is_err() {
            return Err(SourceError::VolumeGone);
        }
        if n == 0 {
            return Ok(WatchBatch::Overflow);
        }
        // SAFETY: the kernel wrote `n` bytes into `buf`, which is at least
        // that long and `u32`-aligned as FILE_NOTIFY_INFORMATION requires.
        let bytes =
            unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), n as usize) };
        Ok(WatchBatch::Changes(parse(bytes)))
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        if self.pending {
            let mut n = 0u32;
            // SAFETY: cancel the read and wait for it to finish, so the
            // kernel no longer references the buffer when it is freed.
            unsafe {
                let _ = CancelIoEx(self.dir, Some(&raw const *self.overlapped));
                let _ =
                    GetOverlappedResult(self.dir, &raw const *self.overlapped, &raw mut n, true);
            }
        }
        // SAFETY: both handles are owned by `self` and closed exactly once.
        unsafe {
            let _ = CloseHandle(self.dir);
            let _ = CloseHandle(self.event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut b = Vec::new();
        b.extend_from_slice(&next.to_le_bytes());
        b.extend_from_slice(&action.to_le_bytes());
        b.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            b.extend_from_slice(&u.to_le_bytes());
        }
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b
    }

    #[test]
    fn parses_notification_chains() {
        let first = record(0, FILE_ACTION_ADDED.0, r"a\b.txt");
        let mut bytes = record(first.len() as u32, FILE_ACTION_ADDED.0, r"a\b.txt");
        bytes.extend(record(0, FILE_ACTION_REMOVED.0, "c"));
        let changes = parse(&bytes);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].kind, WatchKind::Added);
        assert_eq!(changes[0].path.len(), 2);
        assert_eq!(changes[1].kind, WatchKind::Removed);
        assert!(parse(&bytes[..5]).is_empty(), "truncated input is ignored");
    }

    #[test]
    fn watches_a_folder_it_created() {
        let dir = std::env::temp_dir().join(format!("strata-dirwatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = DirWatcher::new(&dir).unwrap();
        assert_eq!(
            w.next(Some(Duration::from_millis(50))).unwrap(),
            WatchBatch::Changes(Vec::new())
        );
        std::fs::write(dir.join("x.txt"), b"x").unwrap();
        let mut seen = false;
        for _ in 0..20 {
            if let WatchBatch::Changes(c) = w.next(Some(Duration::from_millis(100))).unwrap()
                && c.iter().any(|c| c.kind == WatchKind::Added)
            {
                seen = true;
                break;
            }
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen, "the new file was reported");
    }
}
