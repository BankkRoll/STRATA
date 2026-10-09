//! Binary row pages (`STRP`) for the list pane (`list_children`).
//!
//! Format (little-endian, decoded by `ui/src/lib/rows.ts`): a 32-byte
//! header, `count` 64-byte rows, then the UTF-16LE name blob. Children are
//! filtered with the view filters, sorted server-side by any column, and
//! ties break by entry id so pages are stable.

use std::cmp::Ordering;

use serde::Deserialize;
use strata_core::SizeMode;
use strata_index::EntryId;

use crate::classify::safety_code;
use crate::error::{CmdResult, CommandError};
use crate::model::{ViewFilters, VolumeData};

/// `"STRP"` read as a little-endian `u32`.
pub const ROW_PAGE_MAGIC: u32 = u32::from_le_bytes(*b"STRP");
/// Row page version.
pub const ROW_PAGE_VERSION: u16 = 1;
/// Header size.
pub const HEADER_BYTES: usize = 32;
/// Bytes per row.
pub const ROW_STRIDE: usize = 64;
/// Largest page served.
pub const MAX_LIMIT: usize = 5_000;

/// Sortable columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    /// Natural, case-insensitive name.
    Name,
    /// Size in the query's mode.
    Size,
    /// Items below.
    Items,
    /// Modified (directories: newest in subtree).
    Modified,
    /// Created.
    Created,
    /// Accessed.
    Accessed,
    /// Category id.
    Category,
    /// Safety code.
    Safety,
    /// App id.
    App,
}

/// Sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Sort {
    /// Column.
    pub key: SortKey,
    /// Descending.
    pub desc: bool,
}

/// A page request (`RowQuery` in `ui/src/lib/rows.ts`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowQuery {
    /// Volume id.
    pub volume_id: String,
    /// Parent wire id.
    pub parent: u32,
    /// Sort.
    pub sort: Sort,
    /// Size mode.
    pub size_mode: SizeMode,
    /// First row.
    pub offset: usize,
    /// Rows wanted.
    pub limit: usize,
    /// Filters.
    #[serde(default)]
    pub filters: ViewFilters,
}

/// Facts of one row before encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowFacts {
    id: u32,
    parent: u32,
    flags: u32,
    category: u16,
    safety: u8,
    allocated: u64,
    logical: u64,
    items: u32,
    children: u32,
    mtime: u32,
    ctime: u32,
    atime: u32,
    app: u32,
}

fn facts(data: &VolumeData, id: EntryId, parent_wire: u32) -> RowFacts {
    let ix = &data.index;
    let times = ix.times(id);
    let key = data.key_static(id);
    RowFacts {
        id: data.wire(id),
        parent: parent_wire,
        flags: ix.flags(id).0,
        category: (key & 0xF) as u16,
        safety: safety_code(data.class_bits(id)),
        allocated: ix.size(id, SizeMode::Allocated),
        logical: ix.size(id, SizeMode::Logical),
        items: u32::try_from(data.items(id)).unwrap_or(u32::MAX),
        children: u32::try_from(ix.child_count(id)).unwrap_or(u32::MAX),
        mtime: data.modified(id),
        ctime: times.map_or(0, |t| t.created.0),
        atime: times.map_or(0, |t| t.accessed.0),
        app: ix.owner_app(id),
    }
}

/// Builds the page answering `q`.
///
/// # Errors
///
/// An unknown parent id.
pub fn row_page(data: &VolumeData, q: &RowQuery) -> CmdResult<Vec<u8>> {
    let parent = data
        .resolve(q.parent)
        .ok_or_else(|| CommandError::not_found("that folder is no longer in the index"))?;
    let parent_wire = data.wire(parent);
    let filters = data.resolve_filters(&q.filters, q.size_mode);
    let ix = &data.index;
    let kids: Vec<EntryId> = ix
        .children(parent)
        .filter(|&k| filters.keep(data, k))
        .collect();
    let total = kids.len();
    let mut rows: Vec<(RowFacts, String)> = kids
        .iter()
        .map(|&k| (facts(data, k, parent_wire), ix.name_lossy(k)))
        .collect();
    let mode = q.size_mode;
    let size = |r: &RowFacts| match mode {
        SizeMode::Allocated => r.allocated,
        SizeMode::Logical => r.logical,
    };
    let primary = |a: &(RowFacts, String), b: &(RowFacts, String)| -> Ordering {
        let (x, y) = (&a.0, &b.0);
        match q.sort.key {
            SortKey::Name => natural_cmp(&a.1, &b.1),
            SortKey::Size => size(x).cmp(&size(y)),
            SortKey::Items => x.items.cmp(&y.items),
            SortKey::Modified => x.mtime.cmp(&y.mtime),
            SortKey::Created => x.ctime.cmp(&y.ctime),
            SortKey::Accessed => x.atime.cmp(&y.atime),
            SortKey::Category => x.category.cmp(&y.category),
            SortKey::Safety => x.safety.cmp(&y.safety),
            SortKey::App => x.app.cmp(&y.app),
        }
    };
    rows.sort_by(|a, b| {
        let p = primary(a, b);
        let p = if q.sort.desc { p.reverse() } else { p };
        p.then(a.0.id.cmp(&b.0.id))
    });
    let limit = q.limit.min(MAX_LIMIT);
    let start = q.offset.min(total);
    let end = start.saturating_add(limit).min(total);
    // NOTE: names are re-read losslessly from the index (the sort key above
    // is the lossy display string), so unpaired surrogates reach the UI.
    Ok(encode_page(
        parent_wire,
        u32::try_from(total).unwrap_or(u32::MAX),
        u32::try_from(start).unwrap_or(u32::MAX),
        &rows[start..end]
            .iter()
            .map(|(f, _)| (*f, ix.name(EntryId(crate::ids::decode(f.id).1))))
            .collect::<Vec<_>>(),
    ))
}

/// Case-insensitive natural order: digit runs compare numerically.
#[must_use]
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let x: Vec<char> = a.chars().flat_map(char::to_lowercase).collect();
    let y: Vec<char> = b.chars().flat_map(char::to_lowercase).collect();
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if x[i].is_ascii_digit() && y[j].is_ascii_digit() {
            let (xe, ye) = (digit_run_end(&x, i), digit_run_end(&y, j));
            let (m, n) = (trim_zeros(&x[i..xe]), trim_zeros(&y[j..ye]));
            let o = m.len().cmp(&n.len()).then_with(|| m.cmp(n));
            if o != Ordering::Equal {
                return o;
            }
            i = xe;
            j = ye;
        } else {
            if x[i] != y[j] {
                return x[i].cmp(&y[j]);
            }
            i += 1;
            j += 1;
        }
    }
    (x.len() - i).cmp(&(y.len() - j))
}

fn digit_run_end(s: &[char], mut k: usize) -> usize {
    while k < s.len() && s[k].is_ascii_digit() {
        k += 1;
    }
    k
}

fn trim_zeros(s: &[char]) -> &[char] {
    let z = s.iter().take_while(|&&c| c == '0').count();
    &s[z..]
}

fn encode_page(
    parent: u32,
    total: u32,
    offset: u32,
    rows: &[(RowFacts, strata_core::WideName)],
) -> Vec<u8> {
    let names_len: usize = rows.iter().map(|(_, n)| n.len() * 2).sum();
    let names_off = HEADER_BYTES + rows.len() * ROW_STRIDE;
    let mut out = Vec::with_capacity(names_off + names_len);
    out.extend_from_slice(&ROW_PAGE_MAGIC.to_le_bytes());
    out.extend_from_slice(&ROW_PAGE_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    for v in [
        parent,
        total,
        offset,
        rows.len() as u32,
        names_off as u32,
        names_len as u32,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut name_at = 0u32;
    for (r, name) in rows {
        out.extend_from_slice(&r.id.to_le_bytes());
        out.extend_from_slice(&r.parent.to_le_bytes());
        out.extend_from_slice(&r.flags.to_le_bytes());
        out.extend_from_slice(&r.category.to_le_bytes());
        out.push(r.safety);
        out.push(0);
        out.extend_from_slice(&r.allocated.to_le_bytes());
        out.extend_from_slice(&r.logical.to_le_bytes());
        for v in [
            r.items, r.children, r.mtime, r.ctime, r.atime, r.app, name_at,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let len = name.len() as u32;
        out.extend_from_slice(&len.to_le_bytes());
        name_at += len;
    }
    for (_, name) in rows {
        for u in name.units() {
            out.extend_from_slice(&u.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::WideName;

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "a", "file02x"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["a", "file1", "File2", "file02x", "file10"]);
    }

    /// Shared with `ui/src/lib/rows.fixture.test.ts`, which decodes the
    /// same bytes with the UI's `decodeRowPage`. Regenerate with
    /// `STRATA_UPDATE_FIXTURES=1`.
    #[test]
    fn page_matches_shared_ui_fixture() {
        let dir = RowFacts {
            id: 0x4000_0010,
            parent: 0x4000_0001,
            flags: 1 | 1 << 17,
            category: 5,
            safety: 1,
            allocated: 3 * (1 << 40),
            logical: 123_456_789,
            items: 42,
            children: 7,
            mtime: 800_000_000,
            ctime: 700_000_000,
            atime: 0,
            app: 3,
        };
        let file = RowFacts {
            id: 0x4000_0011,
            flags: 1 << 1,
            category: 10,
            safety: 0,
            allocated: 4096,
            logical: 10,
            items: 0,
            children: 0,
            app: 0,
            ..dir
        };
        let page = encode_page(
            0x4000_0001,
            12,
            10,
            &[
                (dir, WideName::from_str_lossless("node_modules")),
                (file, WideName::from_units(vec![0x00E9, 0xD83D, 0x0021])),
            ],
        );
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../ui/src/lib/__fixtures__/rowpage.bin"
        );
        if std::env::var_os("STRATA_UPDATE_FIXTURES").is_some() {
            std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
            std::fs::write(path, &page).unwrap();
        }
        assert_eq!(std::fs::read(path).expect("fixture present"), page);
    }

    #[test]
    fn page_layout_is_byte_exact() {
        let row = RowFacts {
            id: 0x4000_0002,
            parent: 0x4000_0001,
            flags: 1,
            category: 6,
            safety: 1,
            allocated: 0x1_0000_0000,
            logical: 5,
            items: 3,
            children: 2,
            mtime: 10,
            ctime: 11,
            atime: 12,
            app: 7,
        };
        let name = WideName::from_units(vec![0x61, 0xD800]);
        let page = encode_page(0x4000_0001, 9, 4, &[(row, name)]);
        assert_eq!(page.len(), 32 + 64 + 4);
        let u32_at = |o: usize| u32::from_le_bytes(page[o..o + 4].try_into().unwrap());
        assert_eq!(u32_at(0), 0x5052_5453);
        assert_eq!(&page[4..8], &[1, 0, 0, 0]);
        assert_eq!(
            [
                u32_at(8),
                u32_at(12),
                u32_at(16),
                u32_at(20),
                u32_at(24),
                u32_at(28)
            ],
            [0x4000_0001, 9, 4, 1, 96, 4]
        );
        let r = 32;
        assert_eq!(u32_at(r), 0x4000_0002);
        assert_eq!(u32_at(r + 4), 0x4000_0001);
        assert_eq!(u32_at(r + 8), 1);
        assert_eq!(&page[r + 12..r + 16], &[6, 0, 1, 0]);
        assert_eq!(&page[r + 16..r + 24], &0x1_0000_0000u64.to_le_bytes());
        assert_eq!(&page[r + 24..r + 32], &5u64.to_le_bytes());
        assert_eq!(
            (r + 32..r + 64).step_by(4).map(u32_at).collect::<Vec<_>>(),
            [3, 2, 10, 11, 12, 7, 0, 2]
        );
        assert_eq!(&page[96..], &[0x61, 0, 0x00, 0xD8]);
    }
}
