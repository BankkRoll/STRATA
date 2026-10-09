//! Binary layout frames (`STLF`), the container sent over a layout Channel.
//!
//! Format (little-endian, decoded by `ui/src/lib/layout/frame.ts`): a 128-byte
//! header, then five sections (nodes, aggregates, labels, cushions,
//! transitions), each starting 16-byte aligned. Section payloads are
//! `strata-layout` buffers byte for byte.
//!
//! [`write_frame`] is a copy of the reference encoder in
//! `crates/strata-layout/examples/export_fixtures.rs`; a test checks it
//! reproduces the committed UI fixture byte for byte.

use strata_layout::{NodeId, ViewTransform};

/// `"STLF"` read as a little-endian `u32`.
pub const FRAME_MAGIC: u32 = u32::from_le_bytes(*b"STLF");
/// Frame container version.
pub const FRAME_VERSION: u16 = 1;
/// Header size in bytes.
pub const FRAME_HEADER: usize = 128;

/// View kinds as encoded in the frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    /// Squarified treemap.
    Treemap = 0,
    /// Icicle (top-down bars).
    Icicle = 1,
    /// Flame graph (bottom-up bars).
    Flame = 2,
    /// Sunburst.
    Sunburst = 3,
    /// Circle packing.
    Bubbles = 4,
    /// Radial mind map.
    MindMap = 5,
}

/// Header fields that are not buffers.
#[derive(Debug, Clone, Copy)]
pub struct FrameMeta {
    /// Producing layout.
    pub view: ViewKind,
    /// Request sequence number echoed back.
    pub seq: u32,
    /// Root entry id (wire id).
    pub root: NodeId,
    /// Root size in the active size mode.
    pub root_bytes: u64,
    /// Canvas width in device pixels.
    pub width: f32,
    /// Canvas height in device pixels.
    pub height: f32,
    /// Device pixel ratio.
    pub dpr: f32,
    /// Visual-zoom transform the layout was computed under.
    pub transform: ViewTransform,
    /// Sunburst center (0 otherwise).
    pub center: (f32, f32),
    /// Sunburst ring width (0 otherwise).
    pub ring_width: f32,
}

/// Encodes a frame from its metadata and five section payloads.
#[must_use]
pub fn write_frame(meta: &FrameMeta, sections: [&[u8]; 5]) -> Vec<u8> {
    let total = FRAME_HEADER + sections.iter().map(|s| s.len() + 15).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.resize(FRAME_HEADER, 0);
    let mut table = [(0u32, 0u32); 5];
    for (slot, bytes) in table.iter_mut().zip(sections) {
        while !out.len().is_multiple_of(16) {
            out.push(0);
        }
        *slot = (out.len() as u32, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }
    let mut flags = 0u32;
    if !sections[3].is_empty() {
        flags |= 1;
    }
    if !sections[4].is_empty() {
        flags |= 2;
    }
    let h = &mut out[..FRAME_HEADER];
    let put = |h: &mut [u8], off: usize, v: &[u8]| h[off..off + v.len()].copy_from_slice(v);
    put(h, 0, &FRAME_MAGIC.to_le_bytes());
    put(h, 4, &FRAME_VERSION.to_le_bytes());
    put(h, 6, &(meta.view as u16).to_le_bytes());
    put(h, 8, &meta.seq.to_le_bytes());
    put(h, 12, &meta.root.to_le_bytes());
    put(h, 16, &meta.width.to_le_bytes());
    put(h, 20, &meta.height.to_le_bytes());
    put(h, 24, &meta.dpr.to_le_bytes());
    put(h, 28, &flags.to_le_bytes());
    put(h, 32, &meta.transform.scale.to_le_bytes());
    put(h, 40, &meta.transform.tx.to_le_bytes());
    put(h, 48, &meta.transform.ty.to_le_bytes());
    put(h, 56, &meta.center.0.to_le_bytes());
    put(h, 60, &meta.center.1.to_le_bytes());
    put(h, 64, &meta.ring_width.to_le_bytes());
    for (k, (off, len)) in table.iter().enumerate() {
        put(h, 72 + k * 8, &off.to_le_bytes());
        put(h, 76 + k * 8, &len.to_le_bytes());
    }
    put(h, 112, &meta.root_bytes.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    #[test]
    fn header_and_sections_are_aligned() {
        let meta = FrameMeta {
            view: ViewKind::Sunburst,
            seq: 9,
            root: 0x4000_0005,
            root_bytes: 1 << 40,
            width: 800.0,
            height: 600.0,
            dpr: 1.5,
            transform: ViewTransform::IDENTITY,
            center: (400.0, 300.0),
            ring_width: 40.0,
        };
        let a = [1u8; 32];
        let b = [2u8; 24];
        let f = write_frame(&meta, [&a, &b, &[], &[], &[]]);
        assert_eq!(u32_at(&f, 0), 0x464C_5453);
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), 1);
        assert_eq!(u16::from_le_bytes([f[6], f[7]]), 3);
        assert_eq!(u32_at(&f, 8), 9);
        assert_eq!(u32_at(&f, 12), 0x4000_0005);
        assert_eq!(u32_at(&f, 28), 0, "no cushions, no transitions");
        assert_eq!((u32_at(&f, 72), u32_at(&f, 76)), (128, 32));
        assert_eq!((u32_at(&f, 80), u32_at(&f, 84)), (160, 24));
        assert_eq!(
            (u32_at(&f, 88), u32_at(&f, 92)),
            (192, 0),
            "empty sections stay aligned"
        );
        assert_eq!(&f[112..120], &(1u64 << 40).to_le_bytes());
        assert_eq!(f.len(), 192);
    }
}
