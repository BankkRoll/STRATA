//! Shared test utilities. Every file a test creates lives under a dedicated
//! `strata-clean-tests` directory and is removed on drop; junctions are
//! unlinked first so cleanup can never reach their targets.

#![allow(dead_code)]

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use strata_clean::never::{NeverList, NeverListConfig};
use strata_clean::{GuardConfig, SafetyGuard};
use strata_core::known::{KnownFolder, KnownFolders, UserFolders};

const REPARSE: u32 = 0x400;
const DIRECTORY: u32 = 0x10;

/// A unique, self-deleting test directory.
#[derive(Debug)]
pub struct TestDir {
    pub path: PathBuf,
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn base() -> PathBuf {
    // D: has room and is not the system drive; fall back to %TEMP%.
    let d = Path::new(r"D:\");
    if d.exists() {
        PathBuf::from(r"D:\strata-clean-tests")
    } else {
        std::env::temp_dir().join("strata-clean-tests")
    }
}

impl TestDir {
    pub fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = base().join(format!("{tag}-{}-{nanos:x}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub fn file(&self, rel: &str, contents: &[u8]) -> PathBuf {
        let p = self.path.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }

    pub fn dir(&self, rel: &str) -> PathBuf {
        let p = self.path.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

/// Unlinks every reparse point under `root` without following it.
pub fn unlink_reparse_points(root: &Path) {
    // Iterative: test trees can be hundreds of levels deep.
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            let a = m.file_attributes();
            if a & REPARSE != 0 {
                if a & DIRECTORY != 0 {
                    let _ = std::fs::remove_dir(e.path());
                } else {
                    let _ = std::fs::remove_file(e.path());
                }
            } else if a & DIRECTORY != 0 {
                stack.push(e.path());
            }
        }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        unlink_reparse_points(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates a directory junction (no elevation needed).
pub fn junction(link: &Path, target: &Path) {
    let out = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Replaces the directory `dir` with a junction to `target`.
pub fn swap_for_junction(dir: &Path, target: &Path) {
    std::fs::remove_dir_all(dir).unwrap();
    junction(dir, target);
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// An approximation of this machine's known folders from environment
/// variables. Real resolution belongs to the Windows layer.
pub fn real_known_folders() -> KnownFolders {
    let mut k = KnownFolders::default();
    let windir = env_path("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    k.machine.insert(KnownFolder::Windir, windir);
    k.machine.insert(
        KnownFolder::ProgramFiles,
        env_path("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into()),
    );
    if let Some(p) = env_path("ProgramFiles(x86)") {
        k.machine.insert(KnownFolder::ProgramFilesX86, p);
    }
    if let Some(p) = env_path("ProgramData") {
        k.machine.insert(KnownFolder::ProgramData, p);
    }
    if let Some(p) = env_path("PUBLIC") {
        k.machine.insert(KnownFolder::Public, p);
    }
    let profile = env_path("USERPROFILE").unwrap();
    k.machine.insert(
        KnownFolder::UserProfiles,
        profile.parent().unwrap().to_path_buf(),
    );
    let mut me = UserFolders {
        is_current: true,
        ..Default::default()
    };
    me.folders.insert(KnownFolder::UserProfile, profile.clone());
    for (f, var) in [
        (KnownFolder::LocalAppData, "LOCALAPPDATA"),
        (KnownFolder::AppData, "APPDATA"),
        (KnownFolder::Temp, "TEMP"),
    ] {
        if let Some(p) = env_path(var) {
            me.folders.insert(f, p);
        }
    }
    for (f, name) in [
        (KnownFolder::Documents, "Documents"),
        (KnownFolder::Desktop, "Desktop"),
        (KnownFolder::Downloads, "Downloads"),
        (KnownFolder::Pictures, "Pictures"),
        (KnownFolder::Music, "Music"),
        (KnownFolder::Videos, "Videos"),
    ] {
        me.folders.insert(f, profile.join(name));
    }
    k.users.push(me);
    k
}

/// A guard for this machine, built once per test binary.
pub fn guard() -> &'static SafetyGuard {
    static G: OnceLock<SafetyGuard> = OnceLock::new();
    G.get_or_init(|| {
        SafetyGuard::new(GuardConfig {
            known: real_known_folders(),
            install_dirs: vec![],
        })
        .unwrap()
    })
}

/// A purely synthetic never-list (no machine state), for fuzzing.
pub fn synthetic_list() -> NeverList {
    let mut known = KnownFolders::default();
    known
        .machine
        .insert(KnownFolder::Windir, r"C:\Windows".into());
    known
        .machine
        .insert(KnownFolder::ProgramFiles, r"C:\Program Files".into());
    known.machine.insert(
        KnownFolder::ProgramFilesX86,
        r"C:\Program Files (x86)".into(),
    );
    known
        .machine
        .insert(KnownFolder::UserProfiles, r"C:\Users".into());
    known
        .machine
        .insert(KnownFolder::ProgramData, r"C:\ProgramData".into());
    let mut u = UserFolders {
        is_current: true,
        ..Default::default()
    };
    u.folders
        .insert(KnownFolder::UserProfile, r"C:\Users\me".into());
    u.folders.insert(
        KnownFolder::LocalAppData,
        r"C:\Users\me\AppData\Local".into(),
    );
    u.folders
        .insert(KnownFolder::AppData, r"C:\Users\me\AppData\Roaming".into());
    u.folders
        .insert(KnownFolder::Documents, r"D:\Dokumente".into());
    u.folders
        .insert(KnownFolder::Desktop, r"C:\Users\me\Desktop".into());
    known.users.push(u);
    NeverList::new(NeverListConfig {
        known,
        install_dirs: vec![r"C:\Program Files\Strata".into()],
        ..Default::default()
    })
    .unwrap()
}

/// A PowerShell child holding `path` open with no sharing. Killed on drop
/// (it is our own test child).
pub struct Holder(pub Child);

impl Holder {
    pub fn spawn(path: &Path) -> Self {
        let script = format!(
            "$f=[IO.File]::Open('{}','Open','ReadWrite','None'); Start-Sleep 60",
            path.display()
        );
        let child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .spawn()
            .unwrap();
        let start = Instant::now();
        // Wait until the exclusive open has happened.
        while std::fs::OpenOptions::new().read(true).open(path).is_ok() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "child never locked the file"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        Self(child)
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
