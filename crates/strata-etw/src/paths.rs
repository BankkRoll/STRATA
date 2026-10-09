//! NT path normalization and directory interning.
//!
//! Kernel events name files by NT path (`\Device\HarddiskVolume3\Users\me\...`).
//! [`PathMapper`] turns them into the DOS paths the rest of Strata uses
//! (`C:\Users\me\...`) with an injected [`DeviceMap`], so tests supply a
//! synthetic map and the helper builds the real one with
//! [`DeviceMap::current`].
//!
//! Each file path is resolved once, when its name is first seen, into a
//! shared [`FileInfo`] carrying its store hash and an interned [`Dir`], so a
//! write costs two hash-map lookups and no string work.

use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::os::windows::ffi::OsStringExt;
use std::sync::{Arc, Weak};

use strata_store::path_hash;
use strata_win::path::DeviceMap;

/// A directory that activity is attributed to.
#[derive(Debug)]
pub struct DirInfo {
    /// DOS path as first seen (display spelling).
    pub path: Arc<str>,
    /// [`strata_store::path_hash`] of `path`.
    pub hash: u64,
}

/// Shared handle to a [`DirInfo`]; equality and hashing are by identity,
/// which the interner makes equivalent to equality of normalized paths.
#[derive(Debug, Clone)]
pub struct Dir(pub Arc<DirInfo>);

impl PartialEq for Dir {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for Dir {}
impl Hash for Dir {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ptr(&self.0) as usize);
    }
}

/// A resolved file name.
#[derive(Debug)]
pub struct FileInfo {
    /// DOS path (or the NT path when the device is unknown).
    pub path: Arc<str>,
    /// [`strata_store::path_hash`] of `path`.
    pub hash: u64,
    /// Parent directory.
    pub dir: Dir,
}

/// Converts NT paths and interns directories.
#[derive(Debug)]
pub struct PathMapper {
    devices: DeviceMap,
    dirs: HashMap<u64, Weak<DirInfo>>,
}

impl PathMapper {
    /// A mapper over `devices`.
    #[must_use]
    pub fn new(devices: DeviceMap) -> Self {
        Self {
            devices,
            dirs: HashMap::new(),
        }
    }

    /// Replaces the device map (volume arrival/removal).
    pub fn set_devices(&mut self, devices: DeviceMap) {
        self.devices = devices;
    }

    /// Converts an NT path to a DOS path string.
    ///
    /// `\Device\...` goes through the device map, `\??\C:\x` and
    /// `\\?\C:\x` lose their prefix, and anything else (including an unknown
    /// device) is returned unchanged. Unpaired surrogates become U+FFFD.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_etw::paths::PathMapper;
    /// use strata_win::path::DeviceMap;
    /// let m = PathMapper::new(DeviceMap::from_entries([
    ///     (r"\Device\HarddiskVolume3".to_owned(), r"C:\".into()),
    /// ]));
    /// let nt: Vec<u16> = r"\Device\HarddiskVolume3\Users\me\a.txt".encode_utf16().collect();
    /// assert_eq!(m.to_dos(&nt), r"C:\Users\me\a.txt");
    /// ```
    #[must_use]
    pub fn to_dos(&self, nt: &[u16]) -> String {
        const DEVICE: &str = r"\Device\";
        let starts = |p: &str| {
            let p: Vec<u16> = p.encode_utf16().collect();
            nt.len() >= p.len() && eq_ascii_ci(&nt[..p.len()], &p)
        };
        if starts(DEVICE) {
            let os = OsString::from_wide(nt);
            if let Some(p) = self.devices.to_dos(&os) {
                return p.to_string_lossy().into_owned();
            }
        } else if starts(r"\??\UNC\") || starts(r"\\?\UNC\") {
            return format!(r"\\{}", String::from_utf16_lossy(&nt[8..]));
        } else if starts(r"\??\") || starts(r"\\?\") {
            return String::from_utf16_lossy(&nt[4..]);
        }
        String::from_utf16_lossy(nt)
    }

    /// Resolves an NT path into a shared [`FileInfo`].
    pub fn file(&mut self, nt: &[u16]) -> Arc<FileInfo> {
        let path = self.to_dos(nt);
        self.file_from_dos(path)
    }

    /// Resolves an already-DOS path into a shared [`FileInfo`].
    pub fn file_from_dos(&mut self, path: String) -> Arc<FileInfo> {
        let trimmed = path.trim_end_matches('\\');
        let parent = match trimmed.rfind('\\') {
            // NOTE: keep the separator for a volume root so `C:\a` has
            // parent `C:\`, not `C:`.
            Some(i) if i > 0 && trimmed.as_bytes()[i - 1] == b':' => &trimmed[..=i],
            Some(i) if i > 0 => &trimmed[..i],
            _ => trimmed,
        };
        let dir = self.dir(parent);
        let hash = path_hash(&path);
        Arc::new(FileInfo {
            path: path.into(),
            hash,
            dir,
        })
    }

    /// Interns a directory path.
    pub fn dir(&mut self, path: &str) -> Dir {
        let hash = path_hash(path);
        if let Some(d) = self.dirs.get(&hash).and_then(Weak::upgrade) {
            return Dir(d);
        }
        let d = Arc::new(DirInfo {
            path: path.into(),
            hash,
        });
        self.dirs.insert(hash, Arc::downgrade(&d));
        Dir(d)
    }

    /// Forgets directories nothing refers to any more.
    pub fn sweep(&mut self) {
        self.dirs.retain(|_, w| w.strong_count() > 0);
    }

    /// Interned directory count (live and unswept).
    #[must_use]
    pub fn interned_dirs(&self) -> usize {
        self.dirs.len()
    }
}

fn eq_ascii_ci(a: &[u16], b: &[u16]) -> bool {
    let low = |c: u16| {
        if (u16::from(b'A')..=u16::from(b'Z')).contains(&c) {
            c + 32
        } else {
            c
        }
    };
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| low(x) == low(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn mapper() -> PathMapper {
        PathMapper::new(DeviceMap::from_entries([
            (r"\Device\HarddiskVolume3".to_owned(), r"C:\".into()),
            (r"\Device\HarddiskVolume12".to_owned(), r"D:\".into()),
        ]))
    }

    #[test]
    fn device_paths_normalize() {
        let m = mapper();
        assert_eq!(
            m.to_dos(&w(r"\Device\HarddiskVolume3\a\b.txt")),
            r"C:\a\b.txt"
        );
        assert_eq!(m.to_dos(&w(r"\device\harddiskvolume12\x")), r"D:\x");
        // Prefix of a longer device number must not match.
        assert_eq!(
            m.to_dos(&w(r"\Device\HarddiskVolume31\x")),
            r"\Device\HarddiskVolume31\x"
        );
        assert_eq!(m.to_dos(&w(r"\??\C:\x")), r"C:\x");
        assert_eq!(m.to_dos(&w(r"\Device\Mup\srv\share\f")), r"\\srv\share\f");
        let lone = [u16::from(b'C'), u16::from(b':'), u16::from(b'\\'), 0xD800];
        assert_eq!(m.to_dos(&lone), "C:\\\u{FFFD}");
    }

    #[test]
    fn dirs_intern_case_insensitively_and_sweep() {
        let mut m = mapper();
        let a = m.file(&w(r"\Device\HarddiskVolume3\Data\a.bin"));
        let b = m.file_from_dos(r"c:\data\B.bin".into());
        assert_eq!(a.dir, b.dir);
        assert_eq!(&*a.dir.0.path, r"C:\Data");
        let root = m.file_from_dos(r"C:\top.txt".into());
        assert_eq!(&*root.dir.0.path, r"C:\");
        assert_eq!(m.interned_dirs(), 2);
        drop((a, b, root));
        m.sweep();
        assert_eq!(m.interned_dirs(), 0);
    }
}
