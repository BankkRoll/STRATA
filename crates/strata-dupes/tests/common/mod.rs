//! Shared test utilities. Every file a test creates lives in a fresh folder
//! under `D:\strata-dupes-tests` (or `%TEMP%\strata-dupes-tests`), removed
//! on drop.

#![allow(dead_code)]

use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use strata_core::{EntryFlags, FileRef, FileTime};
use strata_dupes::{Candidate, VolumeKey};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn base() -> PathBuf {
    if Path::new(r"D:\").exists() {
        PathBuf::from(r"D:\strata-dupes-tests")
    } else {
        std::env::temp_dir().join("strata-dupes-tests")
    }
}

/// A unique, self-deleting test directory.
#[derive(Debug)]
pub struct TestDir {
    pub path: PathBuf,
}

impl TestDir {
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = base().join(format!("{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub fn file(&self, rel: &str, contents: &[u8]) -> PathBuf {
        let p = self.path.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, contents).unwrap();
        p
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // Clear attributes tests may have set (read-only, offline) first.
        if let Ok(rd) = std::fs::read_dir(&self.path) {
            for e in rd.flatten() {
                let _ = set_attributes(&e.path(), 0x80);
            }
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Deterministic pseudo-random bytes.
pub fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(len + 8);
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

/// File index and attributes, read without recalling content.
pub fn file_info(path: &Path) -> (u64, u32, u32) {
    let f = std::fs::OpenOptions::new()
        .access_mode(0x80 | 0x0010_0000)
        .share_mode(7)
        .custom_flags(0x0010_0000 | 0x0020_0000)
        .open(path)
        .unwrap();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is owned by `f` for the call; `info` is a valid
    // out-parameter.
    unsafe { GetFileInformationByHandle(HANDLE(f.as_raw_handle()), &raw mut info) }.unwrap();
    (
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        info.dwFileAttributes,
        info.nNumberOfLinks,
    )
}

pub fn volume() -> VolumeKey {
    VolumeKey::new(0xD, "test-volume")
}

/// A candidate the way the index would describe `path`.
pub fn candidate(path: &Path) -> Candidate {
    let m = std::fs::symlink_metadata(path).unwrap();
    let (id, attrs, _) = file_info(path);
    Candidate {
        volume: volume(),
        file_ref: FileRef(id),
        path: path.to_path_buf(),
        size: m.file_size(),
        mtime: FileTime(m.last_write_time()),
        flags: EntryFlags::from_win32_attributes(attrs),
    }
}

pub fn set_attributes(path: &Path, attrs: u32) -> windows::core::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW};
    let w: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `w` is a NUL-terminated UTF-16 path that outlives the call.
    unsafe {
        SetFileAttributesW(
            windows::core::PCWSTR(w.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(attrs),
        )
    }
}

/// Builds a `SafetyGuard` from this machine's known folders.
pub fn guard() -> strata_clean::SafetyGuard {
    let known = strata_win::known::known_folders().unwrap();
    strata_clean::SafetyGuard::new(strata_clean::GuardConfig {
        known,
        install_dirs: vec![],
    })
    .unwrap()
}
