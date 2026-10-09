//! Event payload layouts and the bounds-checked field reader.
//!
//! A [`Layout`] is the ordered list of top-level fields of one
//! (provider, event id, version), as declared by the provider manifest. The
//! decoder walks the `UserData` bytes field by field and looks fields up by
//! name, so a newer event version that appends fields decodes unchanged.
//!
//! Layouts come from two places:
//! - built-in tables ([`crate::decode::builtin_layouts`]) transcribed from the
//!   manifests of `Microsoft-Windows-Kernel-File` and
//!   `Microsoft-Windows-Kernel-Process`;
//! - the installed manifest at runtime via TDH ([`crate::tdh`]), which covers
//!   versions this build has never seen.
//!
//! Reading never panics: every offset is checked, and a field whose size
//! cannot be determined (arrays, structs, length-prefixed binary) ends the
//! walk, leaving the fields before it readable.

use std::borrow::Cow;
use std::collections::HashMap;

/// Wire type of one top-level field (the TDH `InType`, reduced to what the
/// decoder can size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    /// NUL-terminated UTF-16 string.
    UnicodeString,
    /// NUL-terminated 8-bit string.
    AnsiString,
    /// 1-byte integer (signed or unsigned).
    Int8,
    /// 2-byte integer.
    Int16,
    /// 4-byte integer, including `HexInt32` and `Boolean` (a 4-byte BOOL).
    Int32,
    /// 8-byte integer, including `HexInt64`.
    Int64,
    /// 4-byte float.
    Float,
    /// 8-byte float.
    Double,
    /// 16-byte GUID.
    Guid,
    /// Pointer-sized value: 4 or 8 bytes depending on the event header.
    Pointer,
    /// 8-byte FILETIME.
    FileTime,
    /// 16-byte SYSTEMTIME.
    SystemTime,
    /// Variable-length SID (8 + 4 × sub-authority count bytes).
    Sid,
    /// A field the reader cannot size on its own; ends the walk.
    Opaque,
}

impl FieldType {
    /// Maps a TDH `InType` value. Unknown or variable types become
    /// [`FieldType::Opaque`].
    #[must_use]
    pub const fn from_tdh_intype(in_type: u16) -> Self {
        match in_type {
            1 => Self::UnicodeString,
            2 => Self::AnsiString,
            3 | 4 => Self::Int8,
            5 | 6 => Self::Int16,
            7 | 8 | 13 | 20 => Self::Int32,
            9 | 10 | 21 => Self::Int64,
            11 => Self::Float,
            12 => Self::Double,
            15 => Self::Guid,
            16 => Self::Pointer,
            17 => Self::FileTime,
            18 => Self::SystemTime,
            19 => Self::Sid,
            _ => Self::Opaque,
        }
    }

    /// Size in bytes when fixed; `None` for strings, SIDs and opaque fields.
    #[must_use]
    pub const fn fixed_size(self, pointer_size: usize) -> Option<usize> {
        match self {
            Self::Int8 => Some(1),
            Self::Int16 => Some(2),
            Self::Int32 | Self::Float => Some(4),
            Self::Int64 | Self::Double | Self::FileTime => Some(8),
            Self::Guid | Self::SystemTime => Some(16),
            Self::Pointer => Some(pointer_size),
            Self::UnicodeString | Self::AnsiString | Self::Sid | Self::Opaque => None,
        }
    }
}

/// One named top-level field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Manifest property name (`FileObject`, `IOSize`, ...).
    pub name: Cow<'static, str>,
    /// Wire type.
    pub ty: FieldType,
}

impl Field {
    /// A field with a static name.
    #[must_use]
    pub const fn new(name: &'static str, ty: FieldType) -> Self {
        Self {
            name: Cow::Borrowed(name),
            ty,
        }
    }
}

/// Ordered top-level fields of one event version.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    /// Fields in payload order.
    pub fields: Vec<Field>,
}

impl Layout {
    /// Builds a layout from (name, type) pairs.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_etw::layout::{FieldType, Layout};
    /// let l = Layout::of(&[("FileKey", FieldType::Pointer), ("FileName", FieldType::UnicodeString)]);
    /// assert_eq!(l.fields.len(), 2);
    /// ```
    #[must_use]
    pub fn of(fields: &[(&'static str, FieldType)]) -> Self {
        Self {
            fields: fields.iter().map(|&(n, t)| Field::new(n, t)).collect(),
        }
    }

    /// Splits `data` into field values. Fields that cannot be located
    /// (truncated payload, or after an opaque field) are absent.
    #[must_use]
    pub fn read<'a>(&self, data: &'a [u8], pointer_size: usize) -> Fields<'a> {
        let mut out = Fields {
            values: Vec::with_capacity(self.fields.len()),
            pointer_size,
        };
        let mut off = 0usize;
        for f in &self.fields {
            let Some(rest) = data.get(off..) else { break };
            let len = match f.ty.fixed_size(pointer_size) {
                Some(n) if n <= rest.len() => n,
                Some(_) => break,
                None => match f.ty {
                    FieldType::UnicodeString => utf16z_len(rest),
                    FieldType::AnsiString => rest.iter().position(|&b| b == 0).map(|p| p + 1),
                    FieldType::Sid => sid_len(rest),
                    _ => None,
                }
                .unwrap_or(rest.len()),
            };
            if f.ty == FieldType::Opaque {
                break;
            }
            out.values.push((f.name.clone(), f.ty, &rest[..len]));
            off += len;
        }
        out
    }
}

/// Byte length including the terminator, or `None` when unterminated (the
/// caller then takes the rest of the payload, as TDH does).
fn utf16z_len(b: &[u8]) -> Option<usize> {
    b.chunks_exact(2)
        .position(|c| c == [0, 0])
        .map(|i| i * 2 + 2)
}

fn sid_len(b: &[u8]) -> Option<usize> {
    let count = usize::from(*b.get(1)?);
    let len = 8 + 4 * count;
    (len <= b.len()).then_some(len)
}

/// Field values of one decoded payload, borrowed from the event buffer.
#[derive(Debug)]
pub struct Fields<'a> {
    values: Vec<(Cow<'static, str>, FieldType, &'a [u8])>,
    pointer_size: usize,
}

impl Fields<'_> {
    fn raw(&self, name: &str) -> Option<(FieldType, &[u8])> {
        self.values
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, t, b)| (*t, *b))
    }

    /// Whether the field was present and readable.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.raw(name).is_some()
    }

    /// An integer, pointer or FILETIME field widened to `u64`.
    #[must_use]
    pub fn u64(&self, name: &str) -> Option<u64> {
        let (ty, b) = self.raw(name)?;
        match (ty, b.len()) {
            (FieldType::Int8, 1) => Some(u64::from(b[0])),
            (FieldType::Int16, 2) => Some(u64::from(u16::from_le_bytes([b[0], b[1]]))),
            (FieldType::Int32 | FieldType::Pointer, 4) => {
                Some(u64::from(u32::from_le_bytes(b.try_into().ok()?)))
            }
            (FieldType::Int64 | FieldType::Pointer | FieldType::FileTime, 8) => {
                Some(u64::from_le_bytes(b.try_into().ok()?))
            }
            _ => None,
        }
    }

    /// A 32-bit field (or the low half of a wider integer field).
    #[must_use]
    pub fn u32(&self, name: &str) -> Option<u32> {
        self.u64(name).map(|v| v as u32)
    }

    /// A UTF-16 string field as code units, without the terminator.
    #[must_use]
    pub fn utf16(&self, name: &str) -> Option<Vec<u16>> {
        let (ty, b) = self.raw(name)?;
        if ty != FieldType::UnicodeString {
            return None;
        }
        let mut units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if let Some(p) = units.iter().position(|&u| u == 0) {
            units.truncate(p);
        }
        Some(units)
    }

    /// Pointer width these fields were read with.
    #[must_use]
    pub const fn pointer_size(&self) -> usize {
        self.pointer_size
    }
}

/// Layout table keyed by (provider tag, event id, version).
#[derive(Debug, Clone, Default)]
pub struct LayoutTable {
    map: HashMap<(Provider, u16, u8), Layout>,
}

/// The providers this crate decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Provider {
    /// `Microsoft-Windows-Kernel-File`.
    KernelFile,
    /// `Microsoft-Windows-Kernel-Process`.
    KernelProcess,
}

impl LayoutTable {
    /// Adds or replaces one layout.
    pub fn insert(&mut self, provider: Provider, id: u16, version: u8, layout: Layout) {
        self.map.insert((provider, id, version), layout);
    }

    /// The layout of one event version.
    #[must_use]
    pub fn get(&self, provider: Provider, id: u16, version: u8) -> Option<&Layout> {
        self.map.get(&(provider, id, version))
    }

    /// Number of layouts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every (provider, id, version) key.
    pub fn keys(&self) -> impl Iterator<Item = (Provider, u16, u8)> + '_ {
        self.map.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn reads_fixed_and_string_fields() {
        let l = Layout::of(&[
            ("A", FieldType::Pointer),
            ("Name", FieldType::UnicodeString),
            ("B", FieldType::Int32),
        ]);
        let mut d = 0x1122_3344_5566_7788u64.to_le_bytes().to_vec();
        d.extend(utf16z(r"\x\y"));
        d.extend(7u32.to_le_bytes());
        let f = l.read(&d, 8);
        assert_eq!(f.u64("A"), Some(0x1122_3344_5566_7788));
        assert_eq!(
            String::from_utf16(&f.utf16("Name").unwrap()).unwrap(),
            r"\x\y"
        );
        assert_eq!(f.u32("B"), Some(7));
        let f32 = l.read(&d[4..], 4);
        assert_eq!(f32.u64("A"), Some(0x1122_3344));
    }

    #[test]
    fn truncation_and_opaque_never_panic() {
        let l = Layout::of(&[
            ("A", FieldType::Int64),
            ("X", FieldType::Opaque),
            ("B", FieldType::Int32),
        ]);
        let d = 5u64.to_le_bytes();
        for n in 0..d.len() {
            let f = l.read(&d[..n], 8);
            assert!(!f.has("A"));
        }
        let f = l.read(&d, 8);
        assert_eq!(f.u64("A"), Some(5));
        assert!(!f.has("B"));
        // Unterminated string takes the rest, odd byte included.
        let s = Layout::of(&[("S", FieldType::UnicodeString)]);
        let f = s.read(&[b'a', 0, b'b'], 8);
        assert_eq!(f.utf16("S").unwrap(), vec![u16::from(b'a')]);
        assert!(
            Layout::of(&[("S", FieldType::Sid)])
                .read(&[1, 9], 8)
                .has("S")
        );
    }

    #[test]
    fn tdh_intype_mapping() {
        assert_eq!(FieldType::from_tdh_intype(1), FieldType::UnicodeString);
        assert_eq!(FieldType::from_tdh_intype(16), FieldType::Pointer);
        assert_eq!(FieldType::from_tdh_intype(13), FieldType::Int32);
        assert_eq!(FieldType::from_tdh_intype(14), FieldType::Opaque);
    }
}
