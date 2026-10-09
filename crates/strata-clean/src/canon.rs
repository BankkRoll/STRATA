//! Lexical path canonicalization. Pure: no file-system access.
//!
//! Every delete request is canonicalized before the never-list sees it. The
//! canonicalizer understands every spelling Win32 accepts for the same object
//! (verbatim `\\?\`, device `\\.\`, NT `\??\`, UNC, volume GUID and NT device
//! roots, mixed and repeated separators, `.`/`..`, trailing dots and spaces,
//! case) and refuses spellings that could address something other than a
//! whole file or directory (alternate data streams, reserved device names,
//! pipes and raw devices).
//!
//! Comparison uses an ordinal, locale-independent uppercase fold per UTF-16
//! unit, which is how NTFS compares names. 8.3 short names, junctions,
//! symlinks and mount points cannot be resolved lexically; the guard resolves
//! those through handles and re-runs this canonicalizer on the result.

use std::ffi::OsStr;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::os::windows::ffi::OsStrExt;

use serde::{Deserialize, Serialize};

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const DOT: u16 = b'.' as u16;
const SPACE: u16 = b' ' as u16;
const COLON: u16 = b':' as u16;

/// Why a path could not be canonicalized. Every variant is a refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PathError {
    /// The path is empty.
    #[error("the path is empty")]
    Empty,
    /// The path is relative, drive-relative (`C:foo`) or rooted on the
    /// current drive (`\foo`); only fully qualified paths are accepted.
    #[error("the path is not fully qualified")]
    Relative,
    /// The path contains a character Win32 forbids in names.
    #[error("the path contains the invalid character U+{0:04X}")]
    InvalidCharacter(u16),
    /// The path names an alternate data stream or attribute (`file:stream`).
    #[error("the path names an alternate data stream")]
    AlternateDataStream,
    /// The path addresses a namespace Strata never deletes in (pipes, raw
    /// disks, consoles, unknown `\\?\` roots).
    #[error("the path is not in a supported namespace")]
    UnsupportedNamespace,
    /// `..` climbs above the root.
    #[error("the path climbs above its root")]
    AboveRoot,
    /// A component is a reserved DOS device name (`NUL`, `COM1`, ...), which
    /// Win32 maps to a device instead of a file.
    #[error("the path contains a reserved device name")]
    ReservedDeviceName,
    /// A verbatim (`\\?\`) path contains `.` or `..`, which NTFS never stores
    /// as names; such a request is malformed.
    #[error("a verbatim path contains a dot segment")]
    DotSegmentInVerbatim,
    /// A `Volume{...}` root is not a well-formed GUID.
    #[error("the volume GUID is malformed")]
    InvalidVolumeGuid,
}

/// One path element, kept both as written and in folded comparison form.
///
/// Equality and hashing use only the folded form.
#[derive(Clone)]
pub struct Name {
    orig: Box<[u16]>,
    folded: Box<[u16]>,
}

impl Name {
    fn new(orig: &[u16]) -> Self {
        Self {
            orig: orig.into(),
            folded: fold(orig),
        }
    }

    /// Builds a name from raw UTF-16 units (as listed by the file system).
    #[must_use]
    pub fn from_units(units: &[u16]) -> Self {
        Self::new(units)
    }

    /// Builds a name from a Rust string.
    #[must_use]
    pub fn from_str_name(s: &str) -> Self {
        Self::new(&s.encode_utf16().collect::<Vec<_>>())
    }

    /// The name as written (normalized, not folded).
    #[must_use]
    pub fn orig(&self) -> &[u16] {
        &self.orig
    }

    /// The ordinal-uppercase folded form used for comparison.
    #[must_use]
    pub fn folded(&self) -> &[u16] {
        &self.folded
    }

    /// Display form (lossy for unpaired surrogates).
    #[must_use]
    pub fn display(&self) -> String {
        String::from_utf16_lossy(&self.orig)
    }

    /// Whether the folded name equals `upper`, which must already be folded
    /// ASCII (e.g. `"WINDOWS"`).
    #[must_use]
    pub fn is(&self, upper: &str) -> bool {
        self.folded.iter().copied().eq(upper.encode_utf16())
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        self.folded == other.folded
    }
}
impl Eq for Name {}
impl Hash for Name {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.folded.hash(state);
    }
}
impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.display())
    }
}

/// The root a canonical path hangs from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Root {
    /// Drive letter, uppercase ASCII (`b'C'`).
    Drive(u8),
    /// Volume GUID root, the braced GUID (`{...}`).
    Volume(Name),
    /// UNC share.
    Unc {
        /// Server name.
        server: Name,
        /// Share name.
        share: Name,
    },
    /// NT device root such as `HarddiskVolume3` (from `\Device\...`).
    Device(Name),
}

/// A fully qualified, normalized path.
///
/// Two paths are equal when their roots and folded components are equal.
///
/// # Example
///
/// ```
/// use strata_clean::canon::CanonicalPath;
/// let a = CanonicalPath::parse(r"\\?\c:\WINDOWS\system32").unwrap();
/// let b = CanonicalPath::parse(r"C:/Windows//./Temp/../System32/").unwrap();
/// assert_eq!(a, b);
/// assert_eq!(b.to_string(), r"C:\Windows\System32");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalPath {
    root: Root,
    components: Vec<Name>,
}

impl CanonicalPath {
    /// Canonicalizes a path given as an OS string.
    ///
    /// # Errors
    ///
    /// Returns a [`PathError`] for anything that is not a fully qualified
    /// path to a whole file or directory.
    pub fn parse(path: impl AsRef<OsStr>) -> Result<Self, PathError> {
        let wide: Vec<u16> = path.as_ref().encode_wide().collect();
        Self::parse_wide(&wide)
    }

    /// Canonicalizes a path given as UTF-16 units (no terminating NUL).
    ///
    /// # Errors
    ///
    /// See [`CanonicalPath::parse`].
    pub fn parse_wide(input: &[u16]) -> Result<Self, PathError> {
        parse(input)
    }

    /// Builds a path from a root and already-validated components.
    #[must_use]
    pub fn from_parts(root: Root, components: Vec<Name>) -> Self {
        Self { root, components }
    }

    /// The root.
    #[must_use]
    pub fn root(&self) -> &Root {
        &self.root
    }

    /// Components below the root.
    #[must_use]
    pub fn components(&self) -> &[Name] {
        &self.components
    }

    /// Whether this is a root (no components).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    /// The parent, or `None` for a root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let (_, rest) = self.components.split_last()?;
        Some(Self {
            root: self.root.clone(),
            components: rest.to_vec(),
        })
    }

    /// The last component, or `None` for a root.
    #[must_use]
    pub fn file_name(&self) -> Option<&Name> {
        self.components.last()
    }

    /// Appends one component.
    #[must_use]
    pub fn join(&self, name: Name) -> Self {
        let mut components = self.components.clone();
        components.push(name);
        Self {
            root: self.root.clone(),
            components,
        }
    }

    /// Appends several components.
    #[must_use]
    pub fn join_all(&self, names: &[Name]) -> Self {
        let mut components = self.components.clone();
        components.extend_from_slice(names);
        Self {
            root: self.root.clone(),
            components,
        }
    }

    /// Whether `self` equals `other` or is one of its ancestors.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        self.root == other.root && other.components.starts_with(&self.components)
    }

    /// Whether `self` is a strict ancestor of `other`.
    #[must_use]
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        self.contains(other) && self.components.len() < other.components.len()
    }

    /// The same path with every component's trailing dots and spaces removed
    /// (empty results dropped), when that differs from `self`.
    ///
    /// Win32 strips trailing dots and spaces in some positions but not others,
    /// and verbatim paths keep them; checking this alias as well means no
    /// variant of the rule can make a protected path look unprotected.
    #[must_use]
    pub fn trimmed_alias(&self) -> Option<Self> {
        let mut changed = false;
        let mut components = Vec::with_capacity(self.components.len());
        for c in &self.components {
            let t = trim_trailing_dots_spaces(&c.orig);
            if t.len() != c.orig.len() {
                changed = true;
            }
            if !t.is_empty() {
                components.push(Name::new(t));
            }
        }
        changed.then(|| Self {
            root: self.root.clone(),
            components,
        })
    }

    /// The verbatim (`\\?\`) UTF-16 form, NUL-terminated, for Win32 calls.
    ///
    /// Verbatim paths bypass `MAX_PATH` and Win32 normalization, which is
    /// what we want after canonicalizing: the object opened is exactly the
    /// one that was checked.
    #[must_use]
    pub fn to_verbatim_wide(&self) -> Vec<u16> {
        let mut out: Vec<u16> = r"\\?\".encode_utf16().collect();
        match &self.root {
            Root::Drive(d) => {
                out.push(u16::from(*d));
                out.push(COLON);
            }
            Root::Volume(g) => {
                out.extend("Volume".encode_utf16());
                out.extend_from_slice(&g.orig);
            }
            Root::Unc { server, share } => {
                out.extend(r"UNC\".encode_utf16());
                out.extend_from_slice(&server.orig);
                out.push(BACKSLASH);
                out.extend_from_slice(&share.orig);
            }
            Root::Device(d) => {
                out.extend(r"GLOBALROOT\Device\".encode_utf16());
                out.extend_from_slice(&d.orig);
            }
        }
        out.push(BACKSLASH);
        for (i, c) in self.components.iter().enumerate() {
            if i > 0 {
                out.push(BACKSLASH);
            }
            out.extend_from_slice(&c.orig);
        }
        out.push(0);
        out
    }

    /// The conventional (non-verbatim) UTF-16 form without a NUL, as Explorer
    /// and the Shell display it.
    #[must_use]
    pub fn to_wide(&self) -> Vec<u16> {
        self.to_string().encode_utf16().collect()
    }
}

impl fmt::Display for CanonicalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            Root::Drive(d) => write!(f, "{}:", char::from(*d))?,
            Root::Volume(g) => write!(f, r"\\?\Volume{}", g.display())?,
            Root::Unc { server, share } => {
                write!(f, r"\\{}\{}", server.display(), share.display())?;
            }
            Root::Device(d) => write!(f, r"\Device\{}", d.display())?,
        }
        if self.components.is_empty() {
            return f.write_str("\\");
        }
        for c in &self.components {
            write!(f, "\\{}", c.display())?;
        }
        Ok(())
    }
}

/// Ordinal uppercase fold, one UTF-16 unit or scalar at a time.
///
/// Only one-to-one mappings are applied (`ß` stays `ß`), mirroring the NTFS
/// `$UpCase` table, which maps unit to unit. Where Unicode maps a non-ASCII
/// letter onto ASCII (`ı` to `I`), folding over-matches; for a deny-list that
/// errs on the side of refusing.
#[must_use]
pub fn fold(units: &[u16]) -> Box<[u16]> {
    let mut out = Vec::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => {
                let mut up = c.to_uppercase();
                let mapped = match (up.next(), up.next()) {
                    (Some(u), None) => u,
                    _ => c,
                };
                let mut buf = [0u16; 2];
                out.extend_from_slice(mapped.encode_utf16(&mut buf));
            }
            Err(e) => out.push(e.unpaired_surrogate()),
        }
    }
    out.into_boxed_slice()
}

fn trim_trailing_dots_spaces(s: &[u16]) -> &[u16] {
    let end = s
        .iter()
        .rposition(|&u| u != DOT && u != SPACE)
        .map_or(0, |i| i + 1);
    &s[..end]
}

fn starts_with_ci(s: &[u16], prefix: &str) -> bool {
    let p: Vec<u16> = prefix.encode_utf16().collect();
    s.len() >= p.len() && fold(&s[..p.len()])[..] == fold(&p)[..]
}

fn eq_ci(s: &[u16], other: &str) -> bool {
    fold(s)[..] == fold(&other.encode_utf16().collect::<Vec<_>>())[..]
}

fn is_ascii_alpha(u: u16) -> bool {
    u8::try_from(u).is_ok_and(|b| b.is_ascii_alphabetic())
}

fn parse(input: &[u16]) -> Result<CanonicalPath, PathError> {
    if input.is_empty() {
        return Err(PathError::Empty);
    }
    if input.contains(&0) {
        return Err(PathError::InvalidCharacter(0));
    }
    let s: Vec<u16> = input
        .iter()
        .map(|&u| if u == SLASH { BACKSLASH } else { u })
        .collect();

    let is_prefix = |p: &str| s.starts_with(&p.encode_utf16().collect::<Vec<_>>());
    if is_prefix(r"\\?\") || is_prefix(r"\??\") {
        return parse_prefixed(&s[4..], true);
    }
    if is_prefix(r"\\.\") {
        return parse_prefixed(&s[4..], false);
    }
    if is_prefix(r"\\") {
        let segs = segments(&s[2..]);
        let (server, share, rest) = match segs.as_slice() {
            [server, share, rest @ ..] => (*server, *share, rest),
            _ => return Err(PathError::UnsupportedNamespace),
        };
        let root = unc_root(server, share)?;
        return build(root, rest, false, ends_with_sep(&s));
    }
    if starts_with_ci(&s, r"\Device\") {
        let segs = segments(&s[8..]);
        let Some((dev, rest)) = segs.split_first() else {
            return Err(PathError::UnsupportedNamespace);
        };
        validate_chars(dev)?;
        return build(Root::Device(Name::new(dev)), rest, false, ends_with_sep(&s));
    }
    if s.len() >= 2 && is_ascii_alpha(s[0]) && s[1] == COLON {
        if s.len() == 2 || s[2] != BACKSLASH {
            return Err(PathError::Relative);
        }
        let letter = (s[0] as u8).to_ascii_uppercase();
        return build(
            Root::Drive(letter),
            &segments(&s[3..]),
            false,
            ends_with_sep(&s),
        );
    }
    Err(PathError::Relative)
}

fn parse_prefixed(rest: &[u16], verbatim: bool) -> Result<CanonicalPath, PathError> {
    let trailing = ends_with_sep(rest);
    let segs = segments(rest);
    let Some((first, tail)) = segs.split_first() else {
        return Err(PathError::UnsupportedNamespace);
    };
    if first.len() == 2 && is_ascii_alpha(first[0]) && first[1] == COLON {
        let letter = (first[0] as u8).to_ascii_uppercase();
        return build(Root::Drive(letter), tail, verbatim, trailing);
    }
    if eq_ci(first, "UNC") {
        let (server, share, rest) = match tail {
            [server, share, rest @ ..] => (*server, *share, rest),
            _ => return Err(PathError::UnsupportedNamespace),
        };
        return build(unc_root(server, share)?, rest, verbatim, trailing);
    }
    if starts_with_ci(first, "Volume{") {
        let guid = &first[6..];
        if !is_braced_guid(guid) {
            return Err(PathError::InvalidVolumeGuid);
        }
        return build(Root::Volume(Name::new(guid)), tail, verbatim, trailing);
    }
    if eq_ci(first, "GLOBALROOT") {
        return match tail {
            [device_kw, dev, rest @ ..] if eq_ci(device_kw, "Device") => {
                validate_chars(dev)?;
                build(Root::Device(Name::new(dev)), rest, verbatim, trailing)
            }
            _ => Err(PathError::UnsupportedNamespace),
        };
    }
    Err(PathError::UnsupportedNamespace)
}

fn unc_root(server: &[u16], share: &[u16]) -> Result<Root, PathError> {
    validate_chars(server)?;
    validate_chars(share)?;
    // A `?` or `.` server would be a device path in disguise.
    if server.iter().all(|&u| u == DOT) || server == [b'?' as u16] {
        return Err(PathError::UnsupportedNamespace);
    }
    Ok(Root::Unc {
        server: Name::new(server),
        share: Name::new(share),
    })
}

fn is_braced_guid(g: &[u16]) -> bool {
    // {xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}
    if g.len() != 38 || g[0] != u16::from(b'{') || g[37] != u16::from(b'}') {
        return false;
    }
    g[1..37].iter().enumerate().all(|(i, &u)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            u == u16::from(b'-')
        } else {
            u8::try_from(u).is_ok_and(|b| b.is_ascii_hexdigit())
        }
    })
}

fn ends_with_sep(s: &[u16]) -> bool {
    s.last() == Some(&BACKSLASH)
}

fn segments(s: &[u16]) -> Vec<&[u16]> {
    s.split(|&u| u == BACKSLASH)
        .filter(|seg| !seg.is_empty())
        .collect()
}

fn validate_chars(seg: &[u16]) -> Result<(), PathError> {
    for &u in seg {
        if u == COLON {
            return Err(PathError::AlternateDataStream);
        }
        if u < 0x20 || matches!(u8::try_from(u), Ok(b'<' | b'>' | b'"' | b'|' | b'?' | b'*')) {
            return Err(PathError::InvalidCharacter(u));
        }
    }
    Ok(())
}

const RESERVED: [&str; 8] = [
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM", "LPT",
];

fn is_reserved_device_name(seg: &[u16]) -> bool {
    let base_end = seg.iter().position(|&u| u == DOT).unwrap_or(seg.len());
    let base = trim_trailing_dots_spaces(&seg[..base_end]);
    let f = fold(base);
    let s = String::from_utf16_lossy(&f);
    if RESERVED[..6].contains(&s.as_str()) {
        return true;
    }
    // COM1-9 / LPT1-9, plus the superscript digits Win32 also maps.
    let mut chars = s.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let digit: Vec<char> = chars.collect();
    (prefix == "COM" || prefix == "LPT")
        && digit.len() == 1
        && matches!(digit[0], '1'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}')
}

fn build(
    root: Root,
    segs: &[&[u16]],
    verbatim: bool,
    trailing_sep: bool,
) -> Result<CanonicalPath, PathError> {
    let mut components: Vec<Name> = Vec::with_capacity(segs.len());
    let last = segs.len().saturating_sub(1);
    for (i, seg) in segs.iter().enumerate() {
        let is_dot = *seg == [DOT];
        let is_dotdot = *seg == [DOT, DOT];
        if is_dot || is_dotdot {
            if verbatim {
                return Err(PathError::DotSegmentInVerbatim);
            }
            if is_dotdot && components.pop().is_none() {
                return Err(PathError::AboveRoot);
            }
            continue;
        }
        validate_chars(seg)?;
        if verbatim {
            components.push(Name::new(seg));
            continue;
        }
        if is_reserved_device_name(seg) {
            return Err(PathError::ReservedDeviceName);
        }
        // Win32 normalization: the final segment (without a trailing
        // separator) loses all trailing dots and spaces; other segments lose
        // one trailing dot.
        let normalized: &[u16] = if i == last && !trailing_sep {
            trim_trailing_dots_spaces(seg)
        } else if seg.last() == Some(&DOT) {
            &seg[..seg.len() - 1]
        } else {
            seg
        };
        if !normalized.is_empty() {
            components.push(Name::new(normalized));
        }
    }
    Ok(CanonicalPath { root, components })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> CanonicalPath {
        CanonicalPath::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn err(s: &str) -> PathError {
        CanonicalPath::parse(s).expect_err(s)
    }

    #[test]
    fn drive_spellings_are_equal() {
        let want = p(r"C:\Windows\System32");
        for s in [
            r"c:\windows\system32",
            r"C:/Windows/System32",
            r"C:\\Windows\\\System32\\",
            r"C:\Windows\.\System32",
            r"C:\Windows\Temp\..\System32",
            r"C:\Windows.\System32",
            r"C:\Windows\System32.",
            r"C:\Windows\System32. . .",
            r"\\?\C:\Windows\System32",
            r"\\?\c:\WINDOWS\SYSTEM32\",
            r"\??\C:\Windows\System32",
            r"\\.\C:\Windows\System32",
            r"//?/C:/Windows/System32",
            r"\\.\c:\windows\.\system32",
        ] {
            assert_eq!(p(s), want, "{s}");
        }
    }

    #[test]
    fn display_and_verbatim_forms() {
        let x = p(r"c:/windows\\temp/");
        assert_eq!(x.to_string(), r"C:\windows\temp");
        assert_eq!(
            String::from_utf16_lossy(&x.to_verbatim_wide()),
            "\\\\?\\C:\\windows\\temp\0"
        );
        assert_eq!(p(r"C:\").to_string(), r"C:\");
        assert!(p(r"C:\").is_root());
        assert!(p(r"C:\a\..\.").is_root());
    }

    #[test]
    fn unc_volume_and_device_roots() {
        let u = p(r"\\Server\Share\a\b");
        assert_eq!(u, p(r"\\?\UNC\server\SHARE\a\b"));
        assert_eq!(u, p(r"//server/share//a/./b"));
        let v = p(r"\\?\Volume{11111111-1111-4111-8111-111111111111}\Windows");
        assert!(matches!(v.root(), Root::Volume(_)));
        assert_eq!(
            v,
            p(r"\\?\VOLUME{11111111-1111-4111-8111-111111111111}\windows")
        );
        let d = p(r"\Device\HarddiskVolume3\Windows");
        assert_eq!(d, p(r"\\?\GLOBALROOT\Device\harddiskvolume3\Windows"));
        assert_eq!(
            String::from_utf16_lossy(&d.to_verbatim_wide()),
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolume3\\Windows\0"
        );
    }

    #[test]
    fn refuses_streams_devices_and_relative() {
        assert_eq!(err(r"C:\a\file::$DATA"), PathError::AlternateDataStream);
        assert_eq!(
            err(r"C:\Windows:$I30:$INDEX_ALLOCATION"),
            PathError::AlternateDataStream
        );
        assert_eq!(err(r"C:\a\b:stream"), PathError::AlternateDataStream);
        assert_eq!(err(r"C:foo"), PathError::Relative);
        assert_eq!(err(r"C:"), PathError::Relative);
        assert_eq!(err(r"\Windows"), PathError::Relative);
        assert_eq!(err(r"Windows\System32"), PathError::Relative);
        assert_eq!(err(""), PathError::Empty);
        assert_eq!(err(r"C:\a\NUL"), PathError::ReservedDeviceName);
        assert_eq!(err(r"C:\a\com1.txt"), PathError::ReservedDeviceName);
        assert_eq!(err(r"C:\a\lpt9 "), PathError::ReservedDeviceName);
        assert_eq!(err(r"\\.\pipe\x"), PathError::UnsupportedNamespace);
        assert_eq!(err(r"\\.\PhysicalDrive0"), PathError::UnsupportedNamespace);
        assert_eq!(
            err(r"\\?\GLOBALROOT\??\C:\x"),
            PathError::UnsupportedNamespace
        );
        assert_eq!(err(r"\\?\C:\Windows\..\x"), PathError::DotSegmentInVerbatim);
        assert_eq!(err(r"C:\.."), PathError::AboveRoot);
        assert_eq!(err(r"C:\a*b"), PathError::InvalidCharacter(u16::from(b'*')));
        assert_eq!(err("C:\\a\u{1}"), PathError::InvalidCharacter(1));
        assert_eq!(err(r"\\?\Volume{nope}\x"), PathError::InvalidVolumeGuid);
        assert_eq!(err(r"\\server"), PathError::UnsupportedNamespace);
        // Verbatim keeps a real file called `nul`.
        assert!(CanonicalPath::parse(r"\\?\C:\a\nul").is_ok());
    }

    #[test]
    fn verbatim_keeps_trailing_dots_but_alias_strips_them() {
        let v = p(r"\\?\C:\Windows.\System32 ");
        assert_ne!(v, p(r"C:\Windows\System32"));
        assert_eq!(v.trimmed_alias().unwrap(), p(r"C:\Windows\System32"));
        assert!(p(r"C:\Windows").trimmed_alias().is_none());
    }

    #[test]
    fn non_final_segments_keep_trailing_spaces() {
        let x = p(r"C:\Windows \System32");
        assert_ne!(x, p(r"C:\Windows\System32"));
        assert_eq!(x.trimmed_alias().unwrap(), p(r"C:\Windows\System32"));
    }

    #[test]
    fn ancestry() {
        let a = p(r"C:\Users");
        let b = p(r"c:\users\me\Documents");
        assert!(a.is_ancestor_of(&b));
        assert!(a.contains(&a));
        assert!(!a.is_ancestor_of(&a));
        assert!(!p(r"D:\Users").contains(&b));
        assert_eq!(b.parent().unwrap(), p(r"C:\Users\me"));
        assert!(p(r"C:\").parent().is_none());
    }

    #[test]
    fn fold_is_ordinal_and_one_to_one() {
        let f = |s: &str| String::from_utf16_lossy(&fold(&s.encode_utf16().collect::<Vec<_>>()));
        assert_eq!(f("windows"), "WINDOWS");
        assert_eq!(f("straße"), "STRAßE");
        assert_eq!(f("ärger"), "ÄRGER");
        // Unpaired surrogates survive folding.
        assert_eq!(
            &*fold(&[0xD800, u16::from(b'a')]),
            &[0xD800, u16::from(b'A')]
        );
    }
}
