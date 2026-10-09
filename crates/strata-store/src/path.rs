//! Path normalization and the 64-bit path hash.
//!
//! Directory aggregates, activity rollups and last-writer rows are keyed by
//! [`path_hash`]: xxh3-64 of the UTF-8 bytes of the normalized path. Any crate
//! that needs to look something up (the ETW tracer, the detail panel) must
//! hash through this function so keys agree.
//!
//! Normalization mirrors how NTFS compares names (case-insensitive) closely
//! enough for keying: `\\?\` prefixes are stripped, `/` becomes `\`, each
//! character is uppercased when its uppercase form is a single character (so
//! `ß` stays `ß`, matching the NTFS upcase table rather than Unicode's
//! `SS`), and a trailing separator is removed except on a drive root.
//!
//! WSL case-sensitive directories containing both `A` and `a` collapse to one
//! key. That is an accepted limitation for history aggregates: such pairs are
//! rare and the combined total is still correct for the parent.

use xxhash_rust::xxh3::xxh3_64;

/// Normalizes a display path into its hashing form.
///
/// # Example
///
/// ```
/// use strata_store::normalize_path;
/// assert_eq!(normalize_path(r"\\?\c:/Users/me/"), r"C:\USERS\ME");
/// assert_eq!(normalize_path("c:\\"), "C:\\");
/// ```
#[must_use]
pub fn normalize_path(path: &str) -> String {
    let stripped = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        // Keep the UNC double separator so `\\server\share` and its verbatim
        // form hash the same.
        return normalize_body(&format!(r"\\{rest}"));
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest
    } else {
        path
    };
    normalize_body(stripped)
}

fn normalize_body(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        let c = if c == '/' { '\\' } else { c };
        let mut up = c.to_uppercase();
        if up.len() == 1 {
            out.extend(up.next());
        } else {
            out.push(c);
        }
    }
    while out.ends_with('\\') && !is_root(&out) {
        out.pop();
    }
    out
}

/// `C:\` or a bare `\`/`\\` prefix: separators that are the whole root.
fn is_root(normalized: &str) -> bool {
    let b = normalized.as_bytes();
    (b.len() == 3 && b[1] == b':' && b[2] == b'\\') || normalized.len() <= 2
}

/// Hash key of a path: xxh3-64 of its [`normalize_path`] form.
///
/// # Example
///
/// ```
/// use strata_store::path_hash;
/// assert_eq!(path_hash(r"C:\Users"), path_hash("c:/users/"));
/// assert_ne!(path_hash(r"C:\Users"), path_hash(r"D:\Users"));
/// ```
#[must_use]
pub fn path_hash(path: &str) -> u64 {
    xxh3_64(normalize_path(path).as_bytes())
}

/// Normalized parent of an already-normalized path, or `None` for a root.
pub(crate) fn normalized_parent(normalized: &str) -> Option<&str> {
    if is_root(normalized) {
        return None;
    }
    let idx = normalized.rfind('\\')?;
    if idx <= 1 {
        return None;
    }
    let b = normalized.as_bytes();
    if idx == 2 && b[1] == b':' {
        Some(&normalized[..3])
    } else {
        Some(&normalized[..idx])
    }
}

/// Hash of the parent directory of `path`, or `None` for a root.
///
/// # Example
///
/// ```
/// use strata_store::{parent_hash, path_hash};
/// assert_eq!(parent_hash(r"C:\Users\me"), Some(path_hash(r"C:\Users")));
/// assert_eq!(parent_hash(r"C:\Users"), Some(path_hash(r"C:\")));
/// assert_eq!(parent_hash(r"C:\"), None);
/// ```
#[must_use]
pub fn parent_hash(path: &str) -> Option<u64> {
    let n = normalize_path(path);
    normalized_parent(&n).map(|p| xxh3_64(p.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_separators_case_and_prefixes() {
        assert_eq!(normalize_path(r"C:\a\B/c"), r"C:\A\B\C");
        assert_eq!(normalize_path(r"\\?\C:\x\"), r"C:\X");
        assert_eq!(normalize_path(r"\\?\UNC\srv\share\d"), r"\\SRV\SHARE\D");
        assert_eq!(normalize_path(r"C:\\"), r"C:\");
    }

    #[test]
    fn multi_char_uppercase_is_left_alone() {
        assert_eq!(normalize_path("C:\\straße"), "C:\\STRAßE");
        assert_eq!(normalize_path("C:\\ñ😀"), "C:\\Ñ😀");
    }

    #[test]
    fn parents() {
        assert_eq!(normalized_parent(r"C:\A\B"), Some(r"C:\A"));
        assert_eq!(normalized_parent(r"C:\A"), Some(r"C:\"));
        assert_eq!(normalized_parent(r"C:\"), None);
        assert_eq!(normalized_parent(r"\\SRV\SHARE"), Some(r"\\SRV"));
        assert_eq!(normalized_parent(r"\\SRV"), None);
        assert_eq!(normalized_parent("relative"), None);
    }

    proptest::proptest! {
        #[test]
        // `?` is excluded because `\\?\\\?\x` legitimately loses one prefix
        // per pass.
        fn normalization_is_idempotent(s in "[^?]{0,40}") {
            let once = normalize_path(&s);
            proptest::prop_assert_eq!(normalize_path(&once), once.clone());
        }
    }
}
