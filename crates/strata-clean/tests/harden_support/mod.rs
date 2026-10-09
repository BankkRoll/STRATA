//! Fixtures for the hardening tests. Every file lives under a directory the
//! test created (`D:\strata-harden-tests\...` when D: exists, otherwise
//! `%TEMP%\strata-harden-...`) and is removed on drop, junctions first.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique, self-deleting directory.
#[derive(Debug)]
pub struct HardenDir {
    pub path: PathBuf,
}

impl HardenDir {
    pub fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let leaf = format!("{tag}-{}-{nanos:x}-{n}", std::process::id());
        let path = if Path::new(r"D:\").exists() {
            PathBuf::from(r"D:\strata-harden-tests").join(leaf)
        } else {
            std::env::temp_dir().join(format!("strata-harden-{leaf}"))
        };
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

impl Drop for HardenDir {
    fn drop(&mut self) {
        crate::common::unlink_reparse_points(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The address of the interface this machine would use to reach the
/// internet, without sending anything (a UDP `connect` only picks a route).
pub fn lan_address() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// `D:\x\y` as `\\{host}\D$\x\y`.
pub fn via_admin_share(host: &str, p: &Path) -> PathBuf {
    let s = p.display().to_string();
    let drive = &s[..1];
    PathBuf::from(format!(r"\\{host}\{drive}$\{}", &s[3..]))
}
