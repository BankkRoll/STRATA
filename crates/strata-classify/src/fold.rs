//! Windows-correct name and path normalization.
//!
//! Every comparison the classifier makes goes through this module:
//! - Case folding is *ordinal simple uppercase*, one UTF-16 unit at a time,
//!   which mirrors how NTFS compares names with its `$UpCase` table. It never
//!   uses locale rules (no Turkish dotless-i special case, no `ß` → `SS`).
//! - Separators are normalized to `\`, repeated separators collapse, and
//!   trailing separators are dropped.
//! - Win32 namespace prefixes are stripped: `\\?\C:\x` → `C:\x`,
//!   `\\?\UNC\srv\share` → `\\srv\share`, `\\.\C:` → `C:`.
//!
//! Unpaired surrogates fold to U+FFFD. That makes two distinct on-disk names
//! compare equal here, which only matters for rule matching; rule patterns
//! never contain surrogates, so no rule can match such a name by accident.

/// Folds one character the way NTFS upcases a UTF-16 unit.
///
/// Characters whose uppercase form is not a single character (`ß`, ligatures)
/// stay unchanged, as in the NTFS upcase table.
#[inline]
fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// Appends the folded form of `units` to `out`.
pub(crate) fn fold_units_into(units: &[u16], out: &mut String) {
    // PERF: names are overwhelmingly ASCII; skip UTF-16 decoding for them.
    if units.iter().all(|&u| u < 0x80) {
        out.reserve(units.len());
        for &u in units {
            // The guard above keeps every unit below 0x80, so the cast is exact.
            out.push((u as u8).to_ascii_uppercase() as char);
        }
        return;
    }
    for c in char::decode_utf16(units.iter().copied()) {
        out.push(fold_char(c.unwrap_or(char::REPLACEMENT_CHARACTER)));
    }
}

/// Appends the folded form of `s` to `out`.
pub(crate) fn fold_str_into(s: &str, out: &mut String) {
    if s.is_ascii() {
        out.reserve(s.len());
        out.extend(s.bytes().map(|b| b.to_ascii_uppercase() as char));
        return;
    }
    out.extend(s.chars().map(fold_char));
}

/// Folds a name (one path component) for comparison.
///
/// # Example
///
/// ```
/// use strata_classify::fold::fold_name;
/// assert_eq!(fold_name("node_modules"), "NODE_MODULES");
/// assert_eq!(fold_name("straße"), "STRAßE");
/// ```
#[must_use]
pub fn fold_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    fold_str_into(s, &mut out);
    out
}

/// Strips Win32 namespace prefixes and returns the remaining path text.
fn strip_namespace(p: &str) -> std::borrow::Cow<'_, str> {
    let bs = |s: &str| s.replace('/', "\\");
    let p = bs(p);
    let lower = p.to_ascii_uppercase();
    if let Some(rest) = lower.strip_prefix(r"\\?\UNC\") {
        let off = p.len() - rest.len();
        return format!(r"\\{}", &p[off..]).into();
    }
    if lower.starts_with(r"\\?\") || lower.starts_with(r"\\.\") || lower.starts_with(r"\??\") {
        return p[4..].to_string().into();
    }
    p.into()
}

/// Splits a path into normalized, folded components.
///
/// Drive paths yield `["C:", "USERS", ...]`. UNC paths yield
/// `["\\SERVER\SHARE", ...]`: the server and share form one root component.
/// `.` components are dropped and `..` pops the previous component (never
/// above the root).
///
/// # Example
///
/// ```
/// use strata_classify::fold::path_components;
/// assert_eq!(
///     path_components(r"\\?\c:\Users\\Me/AppData\"),
///     vec!["C:", "USERS", "ME", "APPDATA"]
/// );
/// assert_eq!(
///     path_components(r"\\?\UNC\nas\share\x"),
///     vec![r"\\NAS\SHARE", "X"]
/// );
/// ```
#[must_use]
pub fn path_components(path: &str) -> Vec<String> {
    split_path(path, true)
}

/// Like [`path_components`] but keeps the original case (for display and
/// filesystem access).
#[must_use]
pub fn path_components_raw(path: &str) -> Vec<String> {
    split_path(path, false)
}

fn split_path(path: &str, fold: bool) -> Vec<String> {
    let push_part = |s: &str, out: &mut String| {
        if fold {
            fold_str_into(s, out);
        } else {
            out.push_str(s);
        }
    };
    let p = strip_namespace(path);
    let mut out: Vec<String> = Vec::new();
    let mut parts: Vec<&str> = Vec::new();
    if let Some(unc) = p.strip_prefix(r"\\") {
        let mut it = unc.split('\\').filter(|s| !s.is_empty());
        let mut root = String::from(r"\\");
        if let Some(server) = it.next() {
            push_part(server, &mut root);
        }
        if let Some(share) = it.next() {
            root.push('\\');
            push_part(share, &mut root);
        }
        out.push(root);
        parts.extend(it);
    } else {
        parts.extend(p.split('\\'));
    }
    let root_len = out.len();
    for part in parts {
        match part {
            "" | "." => {}
            ".." => {
                if out.len() > root_len.max(1) {
                    out.pop();
                }
            }
            _ => {
                let mut s = String::with_capacity(part.len());
                push_part(part, &mut s);
                out.push(s);
            }
        }
    }
    out
}

/// Joins folded components back into a normalized path string.
#[must_use]
pub fn join_components<S: AsRef<str>>(comps: &[S]) -> String {
    let mut s = String::new();
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            s.push('\\');
        }
        s.push_str(c.as_ref());
    }
    if comps.len() == 1 && s.ends_with(':') {
        s.push('\\');
    }
    s
}

/// Normalizes a path for case-insensitive comparison: folded, `\`-separated,
/// namespace prefix stripped, no trailing separator (except a bare drive root,
/// which keeps it: `C:\`).
///
/// # Example
///
/// ```
/// use strata_classify::fold::normalize_path;
/// assert_eq!(normalize_path(r"\\?\C:\Windows\\Temp\"), r"C:\WINDOWS\TEMP");
/// assert_eq!(normalize_path("c:/"), r"C:\");
/// ```
#[must_use]
pub fn normalize_path(path: &str) -> String {
    join_components(&path_components(path))
}

/// The extension of a folded name (text after the last `.`), if any.
///
/// A leading dot (`.gitignore`) is not an extension separator, matching
/// [`strata_core::WideName::extension_units`].
#[must_use]
pub fn extension(folded: &str) -> Option<&str> {
    let dot = folded.rfind('.')?;
    if dot == 0 || dot + 1 == folded.len() {
        return None;
    }
    Some(&folded[dot + 1..])
}

/// The stem of a name: text before the last `.` (the whole name if it has no
/// extension).
#[must_use]
pub fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// Removes a browser-style copy suffix: `setup (2).exe` → `setup.exe`.
///
/// Returns `None` when the name has no ` (N)` suffix before its extension.
#[must_use]
pub fn strip_copy_suffix(name: &str) -> Option<String> {
    let (base, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let base = base.strip_suffix(')')?;
    let open = base.rfind(" (")?;
    let digits = &base[open + 2..];
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let original = &base[..open];
    if original.is_empty() {
        return None;
    }
    Some(format!("{original}{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ordinally() {
        assert_eq!(fold_name("Ünïcödé"), "ÜNÏCÖDÉ");
        // Locale-independent: dotted/dotless i fold by simple mapping only.
        assert_eq!(fold_name("i"), "I");
        assert_eq!(fold_name("ß"), "ß");
        assert_eq!(fold_name("ǆ"), "Ǆ");
    }

    #[test]
    fn folds_units_with_unpaired_surrogate() {
        let mut s = String::new();
        fold_units_into(&[u16::from(b'a'), 0xD800, u16::from(b'b')], &mut s);
        assert_eq!(s, "A\u{FFFD}B");
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path(r"C:\a\.\b\..\c\"), r"C:\A\C");
        assert_eq!(normalize_path(r"C:\.."), r"C:\");
        assert_eq!(normalize_path(r"\\.\C:\x"), r"C:\X");
        assert_eq!(normalize_path(r"\\srv\share\Dir"), r"\\SRV\SHARE\DIR");
        assert_eq!(normalize_path(r"\\?\unc\srv\share"), r"\\SRV\SHARE");
        assert_eq!(normalize_path(r"\\srv\share\.."), r"\\SRV\SHARE");
    }

    #[test]
    fn extensions_and_stems() {
        assert_eq!(extension("A.TAR.GZ"), Some("GZ"));
        assert_eq!(extension(".GITIGNORE"), None);
        assert_eq!(extension("X."), None);
        assert_eq!(stem("archive.zip"), "archive");
        assert_eq!(stem(".env"), ".env");
    }

    #[test]
    fn copy_suffix() {
        assert_eq!(
            strip_copy_suffix("setup (2).exe").as_deref(),
            Some("setup.exe")
        );
        assert_eq!(strip_copy_suffix("notes (12)").as_deref(), Some("notes"));
        assert_eq!(strip_copy_suffix("a (x).exe"), None);
        assert_eq!(strip_copy_suffix("(1).exe"), None);
        assert_eq!(strip_copy_suffix("plain.exe"), None);
    }
}
