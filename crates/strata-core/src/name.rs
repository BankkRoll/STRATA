use std::fmt;

use serde::{Deserialize, Serialize};

/// A filesystem name stored losslessly as UTF-16 code units.
///
/// NTFS names are arbitrary sequences of 16-bit units and may contain unpaired
/// surrogates, so they cannot round-trip through `String`. Identity always
/// uses the raw units; [`WideName::to_string_lossy`] is for display only.
/// Names are never case-folded here: NTFS directories can be case-sensitive.
///
/// # Example
///
/// ```
/// use strata_core::WideName;
/// let n = WideName::from_str_lossless("report.pdf");
/// assert_eq!(n.to_string_lossy(), "report.pdf");
/// assert_eq!(n.extension_units(), Some(&[b'p' as u16, b'd' as u16, b'f' as u16][..]));
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct WideName(Box<[u16]>);

impl WideName {
    /// Wraps raw UTF-16 code units.
    #[must_use]
    pub fn from_units(units: impl Into<Box<[u16]>>) -> Self {
        Self(units.into())
    }

    /// Encodes a Rust string (always valid UTF-16, so lossless).
    #[must_use]
    pub fn from_str_lossless(s: &str) -> Self {
        Self(s.encode_utf16().collect())
    }

    /// Raw code units.
    #[must_use]
    pub fn units(&self) -> &[u16] {
        &self.0
    }

    /// Length in UTF-16 code units.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the name is empty (only the volume root has an empty name).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Display form; unpaired surrogates become U+FFFD.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }

    /// Whether the name contains an unpaired surrogate (display is lossy).
    #[must_use]
    pub fn has_unpaired_surrogate(&self) -> bool {
        char::decode_utf16(self.0.iter().copied()).any(|c| c.is_err())
    }

    /// Code units after the last `.`, if the name has a non-empty extension.
    ///
    /// A leading dot (`.gitignore`) is not an extension separator.
    #[must_use]
    pub fn extension_units(&self) -> Option<&[u16]> {
        let dot = self.0.iter().rposition(|&u| u == u16::from(b'.'))?;
        if dot == 0 || dot + 1 == self.0.len() {
            return None;
        }
        Some(&self.0[dot + 1..])
    }
}

impl fmt::Debug for WideName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

impl fmt::Display for WideName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpaired_surrogate_is_preserved_and_displayed_lossily() {
        let n = WideName::from_units(vec![u16::from(b'a'), 0xD800, u16::from(b'b')]);
        assert!(n.has_unpaired_surrogate());
        assert_eq!(n.units(), &[97, 0xD800, 98]);
        assert_eq!(n.to_string_lossy(), "a\u{FFFD}b");
    }

    #[test]
    fn case_variants_are_distinct() {
        assert_ne!(
            WideName::from_str_lossless("A.txt"),
            WideName::from_str_lossless("a.txt")
        );
    }

    #[test]
    fn extension_rules() {
        let ext = |s: &str| {
            WideName::from_str_lossless(s)
                .extension_units()
                .map(String::from_utf16_lossy)
        };
        assert_eq!(ext("a.tar.gz").as_deref(), Some("gz"));
        assert_eq!(ext(".gitignore"), None);
        assert_eq!(ext("trailing."), None);
        assert_eq!(ext("noext"), None);
    }
}
