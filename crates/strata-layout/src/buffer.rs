//! Binary output buffers shared by every layout.
//!
//! Every buffer is a flat `Vec<u8>` of fixed-stride little-endian records,
//! built in place during layout so [`RecordBuf::as_bytes`] hands the exact
//! bytes to a Tauri Channel with no copy and no serialization step. The
//! frontend reads them with `DataView` or typed arrays and uploads the rect
//! buffers straight into WebGL instance attributes.
//!
//! Byte layouts (all little-endian, offsets in bytes):
//!
//! | Record | Stride | Fields |
//! |---|---|---|
//! | [`RectRecord`] | 32 | `x:f32@0 y:f32@4 w:f32@8 h:f32@12 id:u32@16 color_key:u32@20 parent:u32@24 depth:u16@28 flags:u16@30` |
//! | [`ArcRecord`] | 32 | `a0:f32@0 a1:f32@4 r0:f32@8 r1:f32@12 id:u32@16 color_key:u32@20 parent:u32@24 depth:u16@28 flags:u16@30` |
//! | [`CircleRecord`] | 32 | `cx:f32@0 cy:f32@4 r:f32@8 aux:f32@12 id:u32@16 color_key:u32@20 parent:u32@24 depth:u16@28 flags:u16@30` |
//! | [`AggregateRecord`] | 24 | `record:u32@0 parent:u32@4 dir_id:u32@8 count:u32@12 bytes:u64@16` |
//! | [`LabelRecord`] | 32 | `record:u32@0 id:u32@4 x:f32@8 y:f32@12 w:f32@16 h:f32@20 size:u64@24` |
//! | [`CushionRecord`] | 16 | `kx2:f32@0 ky2:f32@4 kx1:f32@8 ky1:f32@12` |
//! | [`TransitionRecord`](crate::TransitionRecord) | 48 | see [`crate::transition`] |
//!
//! `parent` and `record` fields are record indices into the main buffer of
//! the same layout; [`NO_INDEX`] (`0xFFFF_FFFF`) means "none".

use std::fmt;
use std::marker::PhantomData;

/// Sentinel record index meaning "no record" (root's parent, an aggregate
/// whose block was too small to draw).
pub const NO_INDEX: u32 = u32::MAX;

/// Per-record flag bits (`u16`), shared by every layout's main buffer.
///
/// # Example
///
/// ```
/// use strata_layout::NodeFlags;
/// let f = NodeFlags::DIR | NodeFlags::HAS_HEADER;
/// assert!(f.contains(NodeFlags::DIR));
/// assert!(!f.contains(NodeFlags::AGGREGATE));
/// assert_eq!(f.bits(), 0b11);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NodeFlags(u16);

impl NodeFlags {
    /// The entry is a directory.
    pub const DIR: Self = Self(1 << 0);
    /// Treemap: the rect reserves a header strip (name + size) at its top.
    pub const HAS_HEADER: Self = Self(1 << 1);
    /// "N small items" block aggregating children below the LOD threshold.
    /// Its `id` is the parent directory's id; details live in the aggregate
    /// side table. The renderer hatches it.
    pub const AGGREGATE: Self = Self(1 << 2);
    /// A real entry the user can select, open or queue (every record except
    /// aggregates).
    pub const SELECTABLE: Self = Self(1 << 3);
    /// A directory whose children were not laid out (depth limit, too small,
    /// or outside the viewport). Zooming or drilling in reveals them.
    pub const TRUNCATED: Self = Self(1 << 4);
    /// Geometry was clipped to the visible region; the true rect is larger.
    pub const CLIPPED: Self = Self(1 << 5);

    /// No flags.
    pub const EMPTY: Self = Self(0);

    /// Raw bits as written to the buffer.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Reinterprets raw bits (unknown bits are kept).
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Whether every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for NodeFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for NodeFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for NodeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [(NodeFlags, &str); 6] = [
            (NodeFlags::DIR, "DIR"),
            (NodeFlags::HAS_HEADER, "HAS_HEADER"),
            (NodeFlags::AGGREGATE, "AGGREGATE"),
            (NodeFlags::SELECTABLE, "SELECTABLE"),
            (NodeFlags::TRUNCATED, "TRUNCATED"),
            (NodeFlags::CLIPPED, "CLIPPED"),
        ];
        let mut set = f.debug_set();
        for (flag, name) in NAMES {
            if self.contains(flag) {
                set.entry(&format_args!("{name}"));
            }
        }
        set.finish()
    }
}

/// A fixed-stride little-endian record.
///
/// Implemented by every record type in this crate; [`RecordBuf`] stores
/// them as raw bytes.
pub trait Record: Copy {
    /// Size of one encoded record in bytes.
    const STRIDE: usize;
    /// Encodes into `out`, which is exactly `STRIDE` bytes long.
    fn write(&self, out: &mut [u8]);
    /// Decodes from `b`, which is exactly `STRIDE` bytes long.
    fn read(b: &[u8]) -> Self;
}

/// Growable buffer of encoded records whose bytes are the IPC payload.
///
/// # Example
///
/// ```
/// use strata_layout::{NodeFlags, RecordBuf, RectRecord, Record};
///
/// let mut buf = RecordBuf::<RectRecord>::new();
/// let rec = RectRecord { x: 1.0, w: 2.0, h: 3.0, id: 7, flags: NodeFlags::DIR, ..Default::default() };
/// assert_eq!(buf.push(rec), 0);
/// assert_eq!(buf.as_bytes().len(), RectRecord::STRIDE);
/// assert_eq!(buf.get(0), Some(rec));
/// assert_eq!(&buf.as_bytes()[16..20], &7u32.to_le_bytes());
/// ```
pub struct RecordBuf<R> {
    bytes: Vec<u8>,
    _record: PhantomData<R>,
}

impl<R> fmt::Debug for RecordBuf<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordBuf")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl<R> Clone for RecordBuf<R> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            _record: PhantomData,
        }
    }
}

impl<R> PartialEq for RecordBuf<R> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl<R: Record> Default for RecordBuf<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: Record> RecordBuf<R> {
    /// Creates an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            _record: PhantomData,
        }
    }

    /// Number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len() / R::STRIDE
    }

    /// Whether the buffer holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Appends a record and returns its index.
    pub fn push(&mut self, r: R) -> u32 {
        let index = self.len() as u32;
        let start = self.bytes.len();
        self.bytes.resize(start + R::STRIDE, 0);
        r.write(&mut self.bytes[start..]);
        index
    }

    /// Decodes record `i`, or `None` when out of range.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<R> {
        let start = i.checked_mul(R::STRIDE)?;
        self.bytes.get(start..start + R::STRIDE).map(R::read)
    }

    /// Overwrites record `i`; out-of-range indices are ignored.
    pub(crate) fn set(&mut self, i: usize, r: R) {
        let start = i * R::STRIDE;
        if let Some(slot) = self.bytes.get_mut(start..start + R::STRIDE) {
            r.write(slot);
        }
    }

    /// Iterates over decoded records.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = R> + '_ {
        self.bytes.chunks_exact(R::STRIDE).map(R::read)
    }

    /// The encoded bytes, ready for IPC. Zero-copy.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the buffer and returns the encoded bytes. Zero-copy.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Removes all records, keeping the allocation for the next layout.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    pub(crate) fn raw(&self) -> &[u8] {
        &self.bytes
    }
}

#[inline]
pub(crate) fn rd_f32(b: &[u8], off: usize) -> f32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[off..off + 4]);
    f32::from_le_bytes(a)
}

#[inline]
pub(crate) fn rd_u32(b: &[u8], off: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[off..off + 4]);
    u32::from_le_bytes(a)
}

#[inline]
pub(crate) fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
pub(crate) fn rd_u64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

#[inline]
pub(crate) fn wr(out: &mut [u8], off: usize, v: &[u8]) {
    out[off..off + v.len()].copy_from_slice(v);
}

/// Writes the 16-byte tail shared by the rect, arc and circle records.
#[inline]
fn write_tail(out: &mut [u8], id: u32, color_key: u32, parent: u32, depth: u16, flags: NodeFlags) {
    wr(out, 16, &id.to_le_bytes());
    wr(out, 20, &color_key.to_le_bytes());
    wr(out, 24, &parent.to_le_bytes());
    wr(out, 28, &depth.to_le_bytes());
    wr(out, 30, &flags.bits().to_le_bytes());
}

macro_rules! node_record {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[$fmeta:meta])* $f:ident @ $off:literal ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Default)]
        pub struct $name {
            $( $(#[$fmeta])* pub $f: f32, )*
            /// Entry id from the [`LayoutSource`](crate::LayoutSource); for
            /// aggregates, the parent directory's id.
            pub id: u32,
            /// Opaque color key from the source.
            pub color_key: u32,
            /// Record index of the parent, or [`NO_INDEX`] for the root.
            pub parent: u32,
            /// Depth below the layout root (root = 0).
            pub depth: u16,
            /// Flag bits.
            pub flags: NodeFlags,
        }

        impl Record for $name {
            const STRIDE: usize = 32;
            fn write(&self, out: &mut [u8]) {
                $( wr(out, $off, &self.$f.to_le_bytes()); )*
                write_tail(out, self.id, self.color_key, self.parent, self.depth, self.flags);
            }
            fn read(b: &[u8]) -> Self {
                Self {
                    $( $f: rd_f32(b, $off), )*
                    id: rd_u32(b, 16),
                    color_key: rd_u32(b, 20),
                    parent: rd_u32(b, 24),
                    depth: rd_u16(b, 28),
                    flags: NodeFlags::from_bits(rd_u16(b, 30)),
                }
            }
        }
    };
}

node_record! {
    /// One treemap or icicle rectangle (32 bytes), in device pixels.
    ///
    /// Records are in pre-order: a directory precedes its descendants, and
    /// later records draw on top of earlier ones.
    RectRecord {
        /// Left edge.
        x @ 0,
        /// Top edge.
        y @ 4,
        /// Width.
        w @ 8,
        /// Height.
        h @ 12,
    }
}

impl RectRecord {
    /// The geometry as a [`Rect`](crate::Rect).
    #[must_use]
    pub fn rect(&self) -> crate::Rect {
        crate::Rect::new(self.x, self.y, self.w, self.h)
    }
}

node_record! {
    /// One sunburst sector (32 bytes).
    ///
    /// Angles are radians in `[0, 2Ï€]`, measured clockwise from 12 o'clock in
    /// screen space: a point at angle `a`, radius `r` sits at
    /// `(cx + rÂ·sin a, cy âˆ’ rÂ·cos a)`. Radii are device pixels from the
    /// layout center. The root is the full disc `a0 = 0, a1 = 2Ï€, r0 = 0`.
    ArcRecord {
        /// Start angle.
        a0 @ 0,
        /// End angle (`a1 >= a0`).
        a1 @ 4,
        /// Inner radius.
        r0 @ 8,
        /// Outer radius.
        r1 @ 12,
    }
}

node_record! {
    /// One circle (32 bytes): a circle-packing bubble or a mind-map node.
    CircleRecord {
        /// Center x.
        cx @ 0,
        /// Center y.
        cy @ 4,
        /// Radius.
        r @ 8,
        /// Layout-specific extra: the node's angle in radians for the mind
        /// map (for label orientation and edge routing), `0` for packing.
        aux @ 12,
    }
}

/// Side-table entry for one directory's "N small items" aggregate (24 bytes).
///
/// Every directory whose children were laid out and that had children
/// excluded by LOD (too small) or by having zero size gets exactly one entry,
/// so `sum(emitted children bytes) + bytes == sum(all children bytes)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AggregateRecord {
    /// Index of the hatched aggregate record in the main buffer, or
    /// [`NO_INDEX`] when even the aggregate was below the LOD threshold.
    pub record: u32,
    /// Index of the directory's record in the main buffer.
    pub parent: u32,
    /// Id of the directory.
    pub dir_id: u32,
    /// Number of direct children folded into the aggregate.
    pub count: u32,
    /// Total bytes of those children.
    pub bytes: u64,
}

impl Record for AggregateRecord {
    const STRIDE: usize = 24;
    fn write(&self, out: &mut [u8]) {
        wr(out, 0, &self.record.to_le_bytes());
        wr(out, 4, &self.parent.to_le_bytes());
        wr(out, 8, &self.dir_id.to_le_bytes());
        wr(out, 12, &self.count.to_le_bytes());
        wr(out, 16, &self.bytes.to_le_bytes());
    }
    fn read(b: &[u8]) -> Self {
        Self {
            record: rd_u32(b, 0),
            parent: rd_u32(b, 4),
            dir_id: rd_u32(b, 8),
            count: rd_u32(b, 12),
            bytes: rd_u64(b, 16),
        }
    }
}

/// A label candidate (32 bytes): a record big enough to carry text.
///
/// The box is where text fits: the header strip for directories with a
/// header, the padded interior for leaves. The frontend fetches the name by
/// `id`, ellipsizes it to `w`, and culls collisions in the Canvas2D overlay.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LabelRecord {
    /// Index of the labelled record in the main buffer.
    pub record: u32,
    /// Entry id (the directory id for aggregates).
    pub id: u32,
    /// Text box left edge.
    pub x: f32,
    /// Text box top edge.
    pub y: f32,
    /// Text box width.
    pub w: f32,
    /// Text box height.
    pub h: f32,
    /// Bytes to print next to the name (the aggregate's bytes for aggregates).
    pub size: u64,
}

impl Record for LabelRecord {
    const STRIDE: usize = 32;
    fn write(&self, out: &mut [u8]) {
        wr(out, 0, &self.record.to_le_bytes());
        wr(out, 4, &self.id.to_le_bytes());
        wr(out, 8, &self.x.to_le_bytes());
        wr(out, 12, &self.y.to_le_bytes());
        wr(out, 16, &self.w.to_le_bytes());
        wr(out, 20, &self.h.to_le_bytes());
        wr(out, 24, &self.size.to_le_bytes());
    }
    fn read(b: &[u8]) -> Self {
        Self {
            record: rd_u32(b, 0),
            id: rd_u32(b, 4),
            x: rd_f32(b, 8),
            y: rd_f32(b, 12),
            w: rd_f32(b, 16),
            h: rd_f32(b, 20),
            size: rd_u64(b, 24),
        }
    }
}

/// Cushion surface coefficients for one rect (16 bytes), parallel to the
/// rect buffer (record `i` â†” cushion `i`). See [`crate::cushion`] for the
/// shading formula.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CushionRecord {
    /// Coefficient of `xÂ²`.
    pub kx2: f32,
    /// Coefficient of `yÂ²`.
    pub ky2: f32,
    /// Coefficient of `x`.
    pub kx1: f32,
    /// Coefficient of `y`.
    pub ky1: f32,
}

impl Record for CushionRecord {
    const STRIDE: usize = 16;
    fn write(&self, out: &mut [u8]) {
        wr(out, 0, &self.kx2.to_le_bytes());
        wr(out, 4, &self.ky2.to_le_bytes());
        wr(out, 8, &self.kx1.to_le_bytes());
        wr(out, 12, &self.ky1.to_le_bytes());
    }
    fn read(b: &[u8]) -> Self {
        Self {
            kx2: rd_f32(b, 0),
            ky2: rd_f32(b, 4),
            kx1: rd_f32(b, 8),
            ky1: rd_f32(b, 12),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_record_layout_is_byte_exact() {
        let r = RectRecord {
            x: 1.5,
            y: -2.0,
            w: 3.0,
            h: 4.0,
            id: 0x0102_0304,
            color_key: 9,
            parent: NO_INDEX,
            depth: 3,
            flags: NodeFlags::DIR | NodeFlags::TRUNCATED,
        };
        let mut buf = RecordBuf::new();
        buf.push(r);
        let b = buf.as_bytes();
        assert_eq!(b.len(), 32);
        assert_eq!(&b[0..4], &1.5f32.to_le_bytes());
        assert_eq!(&b[4..8], &(-2.0f32).to_le_bytes());
        assert_eq!(&b[16..20], &[4, 3, 2, 1]);
        assert_eq!(&b[24..28], &[0xFF; 4]);
        assert_eq!(&b[28..30], &[3, 0]);
        assert_eq!(&b[30..32], &[0b1_0001, 0]);
        assert_eq!(buf.get(0), Some(r));
        assert_eq!(buf.get(1), None);
    }

    #[test]
    fn side_tables_round_trip() {
        let a = AggregateRecord {
            record: 5,
            parent: 2,
            dir_id: 77,
            count: 1000,
            bytes: u64::MAX - 1,
        };
        let mut ab = RecordBuf::new();
        ab.push(a);
        assert_eq!(ab.as_bytes().len(), 24);
        assert_eq!(ab.get(0), Some(a));

        let l = LabelRecord {
            record: 1,
            id: 2,
            x: 3.0,
            y: 4.0,
            w: 5.0,
            h: 6.0,
            size: 1 << 40,
        };
        let mut lb = RecordBuf::new();
        lb.push(l);
        assert_eq!(&lb.as_bytes()[24..32], &(1u64 << 40).to_le_bytes());
        assert_eq!(lb.get(0), Some(l));
    }

    #[test]
    fn set_overwrites_in_place() {
        let mut buf = RecordBuf::<CushionRecord>::new();
        buf.push(CushionRecord::default());
        let c = CushionRecord {
            kx2: 1.0,
            ky2: 2.0,
            kx1: 3.0,
            ky1: 4.0,
        };
        buf.set(0, c);
        buf.set(9, c);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.get(0), Some(c));
    }

    #[test]
    fn flags_debug_lists_names() {
        let s = format!("{:?}", NodeFlags::DIR | NodeFlags::CLIPPED);
        assert_eq!(s, "{DIR, CLIPPED}");
    }
}
