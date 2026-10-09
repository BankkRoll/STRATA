//! Thin Windows shell and filesystem layer for entry actions and the detail
//! panel. Every call is read-only except launching programs the user asked
//! for (Open, Open terminal here).
//!
//! - [`open`]: `ShellExecuteW("open")`, what a double-click in Explorer does.
//! - [`reveal`]: `SHOpenFolderAndSelectItems` on the item's PIDL.
//! - [`properties`]: the Windows Properties dialog (`SHObjectProperties`).
//! - [`open_terminal`]: Windows Terminal (`wt -d`), else a console `cmd.exe`.
//! - [`streams`], [`file_times`], [`reparse_target`], [`read_head_tail`]:
//!   live facts for the detail panel the index does not keep.
//!
//! Shell calls need COM on the calling thread; they run on the Tauri main
//! thread (which has an STA) or initialise COM themselves.

use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard, WIN32_FIND_STREAM_DATA,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ILFree, SHOP_FILEPATH, SHObjectProperties, SHOpenFolderAndSelectItems, SHParseDisplayName,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

fn wide(s: &Path) -> Vec<u16> {
    s.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Opens a file or folder with its default handler.
///
/// # Errors
///
/// The shell's failure code.
pub fn open(path: &Path) -> Result<(), String> {
    let p = wide(path);
    // SAFETY: `p` is NUL-terminated and outlives the call; null strings are
    // valid for the optional parameters.
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // NOTE: ShellExecute reports success as a pseudo-handle value above 32.
    if r.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!("Windows could not open it (code {})", r.0 as usize))
    }
}

fn ensure_com() {
    // SAFETY: plain initialisation; an already-initialised thread returns
    // S_FALSE or RPC_E_CHANGED_MODE, both harmless here.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

/// Opens Explorer with `path` selected.
///
/// # Errors
///
/// When the path cannot be parsed or Explorer refuses.
pub fn reveal(path: &Path) -> Result<(), String> {
    ensure_com();
    let p = wide(path);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: `p` is NUL-terminated; `pidl` receives a shell allocation that
    // is freed below.
    unsafe { SHParseDisplayName(PCWSTR(p.as_ptr()), None, &mut pidl, 0, None) }
        .map_err(|e| format!("could not locate it: {e}"))?;
    // SAFETY: `pidl` is a valid absolute item id list; with no child list
    // the shell selects that item in its parent folder.
    let r = unsafe { SHOpenFolderAndSelectItems(pidl, None, 0) };
    // SAFETY: allocated by SHParseDisplayName and not used afterwards.
    unsafe { ILFree(Some(pidl)) };
    r.map_err(|e| format!("Explorer could not show it: {e}"))
}

/// Shows the Windows Properties dialog of `path`.
///
/// # Errors
///
/// When the shell refuses.
pub fn properties(path: &Path) -> Result<(), String> {
    ensure_com();
    let p = wide(path);
    // SAFETY: `p` is NUL-terminated and outlives the call.
    let ok = unsafe {
        SHObjectProperties(
            Some(HWND::default()),
            SHOP_FILEPATH,
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
        )
    };
    if ok.as_bool() {
        Ok(())
    } else {
        Err("Windows could not show its properties".into())
    }
}

/// Opens a terminal in `dir`: Windows Terminal when installed, else a new
/// `cmd.exe` console.
///
/// # Errors
///
/// When neither can be started.
pub fn open_terminal(dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NEW_CONSOLE: u32 = 0x10;
    if Command::new("wt.exe").arg("-d").arg(dir).spawn().is_ok() {
        return Ok(());
    }
    Command::new("cmd.exe")
        .current_dir(dir)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start a terminal: {e}"))
}

/// Full-precision FILETIMEs read from the filesystem: created, modified,
/// accessed. Never opens the file's data.
#[must_use]
pub fn file_times(path: &Path) -> Option<[u64; 3]> {
    let m = std::fs::symlink_metadata(path).ok()?;
    Some([m.creation_time(), m.last_write_time(), m.last_access_time()])
}

/// Target of a symlink or junction.
#[must_use]
pub fn reparse_target(path: &Path) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|t| strip_verbatim(&t.to_string_lossy()))
}

/// Removes a `\\?\` prefix for display.
#[must_use]
pub fn strip_verbatim(p: &str) -> String {
    p.strip_prefix(r"\\?\UNC\")
        .map(|r| format!(r"\\{r}"))
        .or_else(|| p.strip_prefix(r"\\?\").map(str::to_owned))
        .unwrap_or_else(|| p.to_owned())
}

/// Named streams of `path` (excluding the unnamed data stream), with
/// logical sizes. Allocation per stream is not available without opening
/// each stream, so it is not reported.
#[must_use]
pub fn streams(path: &Path) -> Vec<(String, u64)> {
    let p = wide(path);
    let mut data = WIN32_FIND_STREAM_DATA::default();
    // SAFETY: `p` is NUL-terminated; `data` is the documented buffer type
    // for FindStreamInfoStandard.
    let Ok(h) = (unsafe {
        FindFirstStreamW(
            PCWSTR(p.as_ptr()),
            FindStreamInfoStandard,
            (&raw mut data).cast(),
            None,
        )
    }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    loop {
        let len = data
            .cStreamName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cStreamName.len());
        let raw = String::from_utf16_lossy(&data.cStreamName[..len]);
        // Names look like ":name:$DATA"; the unnamed stream is "::$DATA".
        let name = raw
            .strip_suffix(":$DATA")
            .unwrap_or(&raw)
            .trim_start_matches(':')
            .to_owned();
        if !name.is_empty() {
            out.push((name, u64::try_from(data.StreamSize).unwrap_or(0)));
        }
        // SAFETY: `h` is a live find handle; `data` as above.
        if unsafe { FindNextStreamW(h, (&raw mut data).cast()) }.is_err() {
            break;
        }
    }
    close_find(h);
    out
}

fn close_find(h: HANDLE) {
    // SAFETY: `h` came from FindFirstStreamW and is closed exactly once.
    let _ = unsafe { FindClose(h) };
}

/// Reads the first `head` bytes and last `tail` bytes of a regular file for
/// content sniffing. Callers must not pass cloud placeholders or offline
/// files: reading them would trigger a download.
#[must_use]
pub fn read_head_tail(path: &Path, head: usize, tail: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut h = vec![0u8; head.min(usize::try_from(len).unwrap_or(head))];
    f.read_exact(&mut h).ok()?;
    let mut t = Vec::new();
    if len > (head + tail) as u64 {
        f.seek(SeekFrom::End(-(tail as i64))).ok()?;
        t.resize(tail, 0);
        f.read_exact(&mut t).ok()?;
    }
    Some((h, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(strip_verbatim(r"\\?\C:\x"), r"C:\x");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share"), r"\\srv\share");
        assert_eq!(strip_verbatim(r"D:\y"), r"D:\y");
    }

    #[test]
    fn streams_and_times_of_a_temp_file() {
        let dir = std::env::temp_dir().join(format!("strata-app-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        std::fs::write(dir.join("a.txt:extra"), b"0123456789").unwrap();
        assert_eq!(streams(&f), vec![("extra".to_owned(), 10)]);
        assert!(file_times(&f).is_some_and(|t| t[1] > 0));
        let (h, t) = read_head_tail(&f, 3, 2).unwrap();
        assert_eq!(h, b"hel");
        assert!(t.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
