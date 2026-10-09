//! Layout engines for Strata's visual views.
//!
//! Every layout reads a sized tree through [`LayoutSource`], computes
//! geometry in device pixels, and writes compact little-endian records
//! straight into byte buffers ([`RecordBuf`]) that go to the frontend over a
//! Tauri Channel without any further serialization. The frontend uploads
//! them as WebGL2 instance attributes.
//!
//! Responsibilities:
//! - [`layout_treemap`]: nested squarified treemap with padding, header
//!   strips, LOD aggregation ("N small items"), depth limit, viewport culling
//!   under a visual-zoom [`ViewTransform`], and optional cushion
//!   coefficients ([`cushion`]).
//! - [`layout_icicle`]: icicle / flame graph bars.
//! - [`layout_sunburst`]: radial sunburst sectors.
//! - [`layout_pack`]: circle packing (front-chain packing + Welzl enclosing
//!   circles).
//! - [`layout_mindmap`]: depth-limited radial node-link tree.
//! - Picking: [`RectLayout::pick`], [`ArcLayout::pick`],
//!   [`CircleLayout::pick`], plus [`HitGrid`].
//! - [`transition`]: per-id matching of two layouts for animated drill-down
//!   and live size changes.
//!
//! Output is deterministic: the same input and config give bit-identical
//! bytes, independent of the order the source lists children in.
//!
//! Byte formats are documented in [`buffer`].

#![forbid(unsafe_code)]

#[cfg(target_endian = "big")]
compile_error!(
    "strata-layout buffers are little-endian on the wire; big-endian targets are unsupported"
);

pub mod buffer;
mod circles;
pub mod cushion;
mod geom;
mod hierarchy;
mod icicle;
mod mindmap;
mod pack;
mod partition;
mod rects;
mod source;
mod squarify;
mod sunburst;
pub mod transition;
mod treemap;

pub use buffer::{
    AggregateRecord, ArcRecord, CircleRecord, CushionRecord, LabelRecord, NO_INDEX, NodeFlags,
    Record, RecordBuf, RectRecord,
};
pub use circles::{CircleLayout, CircleLayoutKind};
pub use cushion::CushionParams;
pub use geom::{Rect, ViewTransform};
pub use hierarchy::{Hierarchy, NodeKey, Pick};
pub use icicle::{IcicleConfig, IcicleOrientation, layout_icicle, layout_icicle_into};
pub use mindmap::{MindMapConfig, layout_mindmap};
pub use pack::{PackConfig, layout_pack};
pub use rects::{HitGrid, RectLayout, RectLayoutKind};
pub use source::{LayoutSource, NodeId, SyntheticSpec, VecTree};
pub use sunburst::{ArcLayout, SunburstConfig, layout_sunburst};
pub use transition::{
    RecordPair, TransitionKind, TransitionRecord, match_records, transition_rects,
};
pub use treemap::{TreemapConfig, layout_treemap, layout_treemap_into};
