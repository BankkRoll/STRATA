//! Path handling on raw UTF-16.
//!
//! Every path the walker touches is in extended-length form (`\\?\C:\...` or
//! `\\?\UNC\server\share\...`) so that names Win32 would otherwise normalise
//! (trailing dots and spaces, reserved device names such as `CON`) and paths
//! longer than `MAX_PATH` are passed through verbatim. Paths stay as `u16`
//! units end to end, so unpaired surrogates survive.

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const QUESTION: u16 = b'?' as u16;
const DOT: u16 = b'.' as u16;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Whether `p` already uses the `\\?\` verbatim prefix.
fn is_verbatim(p: &[u16]) -> bool {
    p.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH])
}

/// Converts an absolute Win32 path into extended-length form.
///
/// - `C:\x` becomes `\\?\C:\x`.
/// - `\\server\share\x` becomes `\\?\UNC\server\share\x`.
/// - `\\.\C:\x` becomes `\\?\C:\x`.
/// - Verbatim input is returned unchanged.
///
/// Forward slashes in non-verbatim input are converted to backslashes, and a
/// trailing separator is dropped unless the path is a drive root.
pub(crate) fn to_extended(abs: &[u16]) -> Vec<u16> {
    if is_verbatim(abs) {
        return abs.to_vec();
    }
    let norm: Vec<u16> = abs
        .iter()
        .map(|&u| if u == SLASH { BACKSLASH } else { u })
        .collect();
    let mut out = if norm.starts_with(&[BACKSLASH, BACKSLASH, DOT, BACKSLASH]) {
        let mut v = wide(r"\\?\");
        v.extend_from_slice(&norm[4..]);
        v
    } else if norm.starts_with(&[BACKSLASH, BACKSLASH]) {
        let mut v = wide(r"\\?\UNC\");
        v.extend_from_slice(&norm[2..]);
        v
    } else {
        let mut v = wide(r"\\?\");
        v.extend_from_slice(&norm);
        v
    };
    while out.last() == Some(&BACKSLASH) && !is_drive_root(&out) && out.len() > 4 {
        out.pop();
    }
    out
}

/// `\\?\C:\`
fn is_drive_root(ext: &[u16]) -> bool {
    ext.len() == 7 && ext[5] == u16::from(b':') && ext[6] == BACKSLASH
}

/// Appends one component.
pub(crate) fn join(parent: &[u16], name: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(parent.len() + name.len() + 1);
    out.extend_from_slice(parent);
    if out.last() != Some(&BACKSLASH) {
        out.push(BACKSLASH);
    }
    out.extend_from_slice(name);
    out
}

/// Converts an extended path to the NT object-manager form (`\??\...`) that
/// `NtCreateFile` takes. The two prefixes name the same namespace.
pub(crate) fn to_nt(ext: &[u16]) -> Vec<u16> {
    let mut out = ext.to_vec();
    if is_verbatim(&out) {
        out[1] = QUESTION;
    }
    out
}

/// Human-readable form: strips `\\?\` and turns `\\?\UNC\` back into `\\`.
pub(crate) fn display(ext: &[u16]) -> Vec<u16> {
    let unc = wide(r"\\?\UNC\");
    if ext.starts_with(&unc) {
        let mut v = wide(r"\\");
        v.extend_from_slice(&ext[unc.len()..]);
        v
    } else if is_verbatim(ext) {
        ext[4..].to_vec()
    } else {
        ext.to_vec()
    }
}

/// Whether the extended path is a UNC path (always remote).
pub(crate) fn is_unc(ext: &[u16]) -> bool {
    ext.starts_with(&wide(r"\\?\UNC\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[u16]) -> String {
        String::from_utf16_lossy(v)
    }

    #[test]
    fn drive_paths_get_verbatim_prefix() {
        assert_eq!(s(&to_extended(&wide(r"C:\Users\x"))), r"\\?\C:\Users\x");
        assert_eq!(s(&to_extended(&wide(r"C:\"))), r"\\?\C:\");
        assert_eq!(s(&to_extended(&wide(r"C:/a/b/"))), r"\\?\C:\a\b");
    }

    #[test]
    fn unc_and_device_paths() {
        assert_eq!(
            s(&to_extended(&wide(r"\\srv\share\dir"))),
            r"\\?\UNC\srv\share\dir"
        );
        assert_eq!(s(&to_extended(&wide(r"\\.\C:\x"))), r"\\?\C:\x");
        let verbatim = wide(r"\\?\C:\trailing. ");
        assert_eq!(to_extended(&verbatim), verbatim);
    }

    #[test]
    fn join_avoids_double_separator() {
        assert_eq!(s(&join(&wide(r"\\?\C:\"), &wide("a"))), r"\\?\C:\a");
        assert_eq!(s(&join(&wide(r"\\?\C:\a"), &wide("b"))), r"\\?\C:\a\b");
    }

    #[test]
    fn nt_and_display_forms() {
        assert_eq!(s(&to_nt(&wide(r"\\?\C:\a"))), r"\??\C:\a");
        assert_eq!(s(&to_nt(&wide(r"\\?\UNC\s\x"))), r"\??\UNC\s\x");
        assert_eq!(s(&display(&wide(r"\\?\C:\a"))), r"C:\a");
        assert_eq!(s(&display(&wide(r"\\?\UNC\s\x"))), r"\\s\x");
        assert!(is_unc(&wide(r"\\?\UNC\s\x")));
        assert!(!is_unc(&wide(r"\\?\C:\")));
    }

    #[test]
    fn unpaired_surrogates_pass_through() {
        let mut p = wide(r"C:\");
        p.push(0xD800);
        let ext = to_extended(&p);
        assert_eq!(*ext.last().unwrap(), 0xD800);
        assert_eq!(*join(&ext, &[0xDC00]).last().unwrap(), 0xDC00);
    }
}
