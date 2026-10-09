# Track: layout (`crates/strata-layout`)

Layout engines for every visual view (SPEC §16.1, §16.2), producing compact little-endian
binary buffers for WebGL2 instancing (SPEC §3), plus picking and transition matching.

`#![forbid(unsafe_code)]`, no runtime dependencies (dev: `proptest`, `criterion`).

## Status

| Item | State |
|---|---|
| Squarified treemap: nesting, padding, header strips, DPI-aware config | Done |
| LOD: no rect under `min_px`, per-directory "N small items" aggregate + side table | Done |
| Depth limit, visual-zoom transform, viewport culling and clipping | Done |
| Deterministic output (size desc, id tie-break; order-independent) | Done, proptested |
| Binary buffers: rects, aggregates, labels, cushions, transitions | Done, byte-exact tests |
| Picking (<1 ms at 1M rects), ancestor chain | Done (descent + wide-dir grids; `HitGrid` alternative) |
| Transitions: per-id matching with camera extrapolation | Done |
| Cushion coefficients (optional parallel buffer) | Done |
| Sunburst, icicle/flame, circle packing, mind map (radial node-link) | Done |
| Proptest invariants for treemap / sunburst / icicle / pack / mind map | Done |
| Criterion benchmarks | Done (numbers below) |

## Public API

```rust
// Input: implemented by strata-index for the live tree; VecTree for tests/benches.
pub type NodeId = u32;
pub trait LayoutSource {
    fn size(&self, id: NodeId) -> u64;                          // active SizeMode
    fn children(&self, id: NodeId, out: &mut Vec<(NodeId, u64)>); // append, any order
    fn is_dir(&self, id: NodeId) -> bool;
    fn color_key(&self, id: NodeId) -> u32;                     // opaque to layout
}
pub struct VecTree;            // ROOT = 0; add_dir/add_file/synthetic/shuffle_children
pub struct SyntheticSpec;      // nodes, mean_fanout, dir_ratio, max_depth, seed

// Treemap
pub struct TreemapConfig { viewport, view, padding, header_height, header_min_width,
                           header_min_height, min_px, max_depth, label_min_width,
                           label_min_height, cushion: Option<CushionParams> }
TreemapConfig::new(width_px, height_px, dpi_scale) -> TreemapConfig   // DPI-scaled defaults
TreemapConfig::with_view(ViewTransform) -> TreemapConfig
pub struct ViewTransform { scale: f64, tx: f64, ty: f64 }   // screen = layout*scale + t
ViewTransform::zoom_about(scale, cx, cy)
layout_treemap(&src, root, &cfg) -> RectLayout
layout_treemap_into(&src, root, &cfg, &mut RectLayout)     // reuses allocations

// Other views (M9)
layout_icicle(&src, root, &IcicleConfig) -> RectLayout      // orientation TopDown | BottomUp (flame)
layout_sunburst(&src, root, &SunburstConfig) -> ArcLayout
layout_pack(&src, root, &PackConfig) -> CircleLayout
layout_mindmap(&src, root, &MindMapConfig) -> CircleLayout

// Output containers
RectLayout   { rects(), aggregates(), labels(), cushions(), subtree_end(i), hit_test(x,y), pick(x,y) }
ArcLayout    { arcs(), aggregates(), center(), ring_width(), subtree_end(i), hit_test, pick }
CircleLayout { circles(), aggregates(), labels(), kind(), subtree_end(i), hit_test, pick }
RecordBuf<R> { len(), get(i), iter(), as_bytes() -> &[u8] /* zero-copy */, into_bytes() }
trait Hierarchy { len, id(i), parent(i), depth(i), flags(i), key(i), ancestor_ids(i, &mut Vec) }
Pick { index: u32, id: NodeId, flags: NodeFlags, ancestors: Vec<NodeId> /* root first */ }
HitGrid::build(&RectLayout, cell_px) / .hit_test(&layout, x, y)

// Transitions
transition_rects(&old: RectLayout, &new: RectLayout) -> RecordBuf<TransitionRecord>
match_records(&old, &new) -> Vec<RecordPair>   // any two layouts implementing Hierarchy
```

All geometry is in **device pixels**. The frontend sends the canvas size in device pixels plus
`devicePixelRatio`; on a DPI change it relayouts with the new values.

## Buffer formats (byte-exact, little-endian)

Every buffer is a `Vec<u8>` of fixed-stride records, built in place during layout; `as_bytes()`
hands it to a Tauri Channel with no copy. `NO_INDEX = 0xFFFF_FFFF` means "none".
Record indices (`parent`, `record`) point into the same layout's main buffer.

### Main node records (32 bytes, shared tail)

All three node record types share bytes 16..32, so parent chains, flags and ids are read the
same way for every view.

| Offset | Type | RectRecord (treemap, icicle) | ArcRecord (sunburst) | CircleRecord (pack, mind map) |
|---|---|---|---|---|
| 0 | f32 | `x` (left) | `a0` start angle | `cx` |
| 4 | f32 | `y` (top) | `a1` end angle | `cy` |
| 8 | f32 | `w` | `r0` inner radius | `r` |
| 12 | f32 | `h` | `r1` outer radius | `aux` (mind map: node angle; pack: 0) |
| 16 | u32 | `id` | `id` | `id` |
| 20 | u32 | `color_key` | `color_key` | `color_key` |
| 24 | u32 | `parent` record index (`NO_INDEX` for root) | same | same |
| 28 | u16 | `depth` (root = 0) | same | same |
| 30 | u16 | `flags` | same | same |

WebGL2: one `ARRAY_BUFFER` with stride 32, `vertexAttribDivisor(…, 1)`:
`vec4 geom` at offset 0 (`FLOAT`), `uvec4 meta` at offset 16 (`vertexAttribIPointer`,
`UNSIGNED_INT`) gives `id, color_key, parent, depth | flags << 16`.

Records are in **pre-order**: a parent precedes its descendants, and later records draw on top
of earlier ones. Index 0 is the layout root when anything was emitted.

Sunburst angles are radians in `[0, 2π]`, clockwise from 12 o'clock in screen space: a point at
angle `a`, radius `r` is `(cx + r·sin a, cy − r·cos a)`; `center()` and `ring_width()` come with
the layout (send them in the control message). The root record is the full disc.

### Flags (u16)

| Bit | Name | Meaning |
|---|---|---|
| 0 (`0x01`) | `DIR` | Directory |
| 1 (`0x02`) | `HAS_HEADER` | Treemap: top strip of `header_height` reserved for name + size |
| 2 (`0x04`) | `AGGREGATE` | "N small items" block; `id` = parent dir id; hatch it |
| 3 (`0x08`) | `SELECTABLE` | Real entry (every record except aggregates) |
| 4 (`0x10`) | `TRUNCATED` | Directory whose children were not laid out (depth limit, too small, off-screen) |
| 5 (`0x20`) | `CLIPPED` | Treemap: geometry clipped to the viewport (true rect is larger) |

### AggregateRecord (24 bytes)

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | `record`: index of the hatched block, or `NO_INDEX` if even it was below `min_px` |
| 4 | u32 | `parent`: index of the directory's record |
| 8 | u32 | `dir_id` |
| 12 | u32 | `count`: direct children folded (LOD-small + zero-byte; mind map: beyond top-K) |
| 16 | u64 | `bytes`: their total |

Invariant (proptested): for every laid-out directory,
`Σ bytes(emitted children) + aggregate.bytes = Σ bytes(all children)` and the counts add up.

### LabelRecord (32 bytes)

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | `record` index |
| 4 | u32 | `id` (dir id for aggregates) |
| 8 | f32 | `x` text box |
| 12 | f32 | `y` |
| 16 | f32 | `w` |
| 20 | f32 | `h` |
| 24 | u64 | `size` bytes to print (aggregate bytes for aggregates) |

Treemap: the header strip for `HAS_HEADER` dirs, else the padded interior of leaves/truncated
dirs, clipped to the viewport, only when at least `label_min_width × label_min_height`.
Icicle: the inset bar. Pack: the inscribed square of leaf circles. The frontend fetches names by
id, ellipsizes, and does collision culling in the Canvas2D overlay.

### CushionRecord (16 bytes, optional, parallel to rects)

`kx2:f32@0 ky2:f32@4 kx1:f32@8 ky1:f32@12`. Height field
`z = kx2·x² + kx1·x + ky2·y² + ky1·y` in the same device-pixel space as the rects.
Fragment shader:

```glsl
float nx = -(2.0 * kx2 * x + kx1);
float ny = -(2.0 * ky2 * y + ky1);
float cosa = (nx * L.x + ny * L.y + L.z) / sqrt(nx * nx + ny * ny + 1.0);
float I = Ia + (1.0 - Ia) * max(0.0, cosa);   // L = normalize(-1,-1,10), Ia = 0.15
```

Only emitted when `TreemapConfig::cushion` is `Some`, so flat styling pays nothing.

### TransitionRecord (48 bytes)

| Offset | Type | Field |
|---|---|---|
| 0..16 | 4×f32 | `from` x, y, w, h |
| 16..32 | 4×f32 | `to` x, y, w, h |
| 32 | u32 | `id` |
| 36 | u32 | `old_index` (or `NO_INDEX`) |
| 40 | u32 | `new_index` (or `NO_INDEX`) |
| 44 | u16 | `kind`: 0 stay, 1 appear (fade in), 2 disappear (fade out) |
| 46 | u16 | `flags` (new record's, old's when disappearing) |

Order: disappearing records first, then every new record in new pre-order. Unmatched records
move along the camera transform between the two roots (drill-down: siblings fly off along the
zoom, new deep entries grow from where they would have been inside the old block; same root:
fade in place). For arcs and circles, `match_records` returns index pairs and the frontend
tweens the record fields directly.

## Algorithms

- **Squarify** (`squarify.rs`): Bruls/Huizing/van Wijk greedy rows, O(n) after the sort; rows
  snap to the remaining edge so siblings tile exactly. Edges are computed in `f64` and converted
  to `f32` per edge, so siblings share edges bit-exactly.
- **Treemap** (`treemap.rs`): explicit-stack pre-order traversal (no recursion; real directory
  chains can exceed the 1 MiB Windows main-thread stack). LOD cut by binary search on the
  sorted sizes, then squarify; if a kept child is thinner than `min_px` the cut moves to it and
  squarify repeats (geometric shrink after 4 rounds bounds the worst case). Subtrees outside
  the viewport are skipped entirely, so zoomed layouts cost only what is on screen.
- **Circle packing** (`pack.rs`): d3-hierarchy `packSiblings` front-chain port (Wang et al.)
  plus Welzl's enclosing circle with a deterministic shuffle and a safe fallback. Top-down: each
  directory packs children with radii `√(size/largest)` and scales into its own circle minus
  padding, so LOD and depth limits skip invisible work. Children that cannot reach `min_px` even
  at the upper-bound scale `R/√Σr²` are aggregated before packing.
- **Sunburst / icicle** (`partition.rs`): shared proportional 1-D slicing with LOD cut on arc
  length / bar width.
- **Mind map** (`mindmap.rs`): top-K children per node + "N more", leaf-count weighted wedges,
  rings by depth, node radius by `√(size/root)`.
- **Picking**: top-down descent over the pre-order buffer using subtree skip pointers.
  Directories with more than 256 emitted children get a per-directory uniform grid at layout
  time, so a query is O(depth × entries per cell) whatever the fan-out. Sunburst picks by
  polar ring then angle; icicle by row then x-range; mind map by top-most containing circle.

## Tests

- 44 unit tests, 23 doctests, 8 proptest suites (192–256 cases each) in
  `tests/treemap_invariants.rs` and `tests/views_invariants.rs`:
  - treemap: child area ∝ size within the parent's content box (padding + header removed);
    no sibling overlap; children inside parent content box; every rect ≥ `min_px`; LOD
    aggregate accounts for all excluded bytes and counts; skip pointers bracket descendants;
    all finite; picking returns a containing rect none of whose children contain the point,
    matches `HitGrid`, and returns the full ancestor chain; labels point at real records.
  - determinism: same input twice and with every child list shuffled give identical bytes
    (rects, aggregates, labels, cushions; sunburst, icicle, pack, mind map).
  - sunburst: angles nested and proportional, rings contiguous, LOD by arc length, accounting,
    picking the middle of each sector returns it.
  - icicle/flame: next-row placement, proportional widths, no overlap, accounting, picking.
  - pack: no overlapping sibling circles, children inside parent minus padding, radii ∝ √size,
    diameter ≥ `min_px`, accounting, picking the center returns the node or a descendant.
  - mind map: top-K respected, accounting, finite, depth-limited.
- Degenerate cases (unit): single child, all-zero sizes, one huge + 10k tiny, 1M equal tiny
  files, 0×0 / NaN / negative viewports, extreme aspect ratios (100000×3), NaN config values
  and NaN zoom transforms, depth limit, collinear circles in the enclosing-circle solver.

## Benchmarks

Machine: dev box (see `docs/PROGRESS.md`), release profile, criterion, single thread.
Synthetic tree: `SyntheticSpec { mean_fanout: 16, dir_ratio: 0.2, max_depth: 12, seed: 42 }`,
log-uniform sizes 1 B–4 GiB with 2% zero-byte files. Viewport 2560×1440 at 1.5 DPI scale.

1M-node tree depth: 10.

| Benchmark | Emitted | Time (median) | Target |
|---|---|---|---|
| Treemap, 100k subtree, default LOD | 20,607 rects | **3.2 ms** | ≤ 50 ms |
| Treemap, 100k subtree, LOD ~off (`min_px` 0.01, no padding) | 58,674 rects | **7.6 ms** | ≤ 50 ms |
| Treemap, 1M tree, default LOD | 39,355 rects | **6.8 ms** | 60 fps |
| Treemap, 1M tree, default LOD + cushions | 39,355 rects | 7.1 ms | |
| Treemap, 1M tree, 40× visual zoom (culled) | 280 rects | 47 µs | |
| Treemap, single flat dir of 1M files, LOD ~off | 1,000,001 rects | 191 ms | (pathological; see notes) |
| Pick (descent), 503k-rect layout | per query | **0.12 µs** | < 1 ms |
| Pick + ancestor chain, 503k-rect layout | per query | 0.29 µs | < 1 ms |
| Pick, `HitGrid`, 503k-rect layout | per query | 0.09 µs | |
| `HitGrid::build`, 503k rects | | 21 ms | |
| Pick (descent + wide-dir grid), flat 1M rects | per query | **0.20 µs** (was 950 µs before wide-dir grids) | < 1 ms |
| Pick, `HitGrid`, flat 1M rects | per query | 0.32 µs | |
| Sunburst, 1M tree (6 rings) | 1,459 arcs | 0.39 ms | |
| Icicle, 1M tree (8 rows) | | 1.8 ms | |
| Circle packing, 1M tree | 1,022 circles | 0.70 ms | |
| Circle packing, 100k tree | | 0.77 ms | |
| Mind map, 1M tree (depth 3, top 12) | | 19 µs | |
| `transition_rects`, drill-down 39k → 39k rects | | 7.4 ms | |

Notes: the flat-1M case emits a million rects (each child gets ≥ 0.01 px); with the default
`min_px = 1` the same folder is mostly aggregated. Its cost is dominated by sorting 1M children
and the squarify LOD rounds. The 100k drill-down target is met by 6×–15×. Times are for
`VecTree`; the index's `children()` cost adds on top of them.

## Decisions

- **Ids are `u32`** (`NodeId`), matching the index (SPEC §9.1) and the wire format.
- **Layout in screen space with `f64`**, converting to `f32` per edge only on write. Deep visual
  zoom puts the root at millions of pixels; `f32` would lose whole pixels there. Emitted rects
  are clipped to the viewport (flag `CLIPPED`) so on-wire coordinates stay small.
- **Buffers are built directly as bytes**, not `#[repr(C)]` structs cast with `bytemuck`:
  `bytemuck`/`zerocopy` derives emit `unsafe impl`, which `#![forbid(unsafe_code)]` rejects.
  Building in place is still zero-copy at the IPC boundary. Big-endian targets fail to compile.
- **Record stride 32 for all node types** (one 16-byte geometry vec4 + one 16-byte meta uvec4):
  aligned for WebGL attributes, and shared parsing of `id/parent/depth/flags`.
- **No rayon.** Single-threaded layout is far below budget (see benchmarks); parallel subtrees
  would add index fix-ups to the pre-order buffer for no measurable gain. Revisit only if real
  index-backed sources are much slower than `VecTree`.
- **Picking: descent + per-wide-directory grids** rather than a global grid. Descent needs no
  build step and is as fast as the global grid on normal trees; the global grid costs ~20 ms
  to build per layout. The wide-directory grids fix the only bad case (huge flat folders).
- **Zero-size children** are excluded from area and counted in the aggregate's `count`.
- **Aggregates carry the directory's id** (flag `AGGREGATE`), never a synthetic id, so matching
  across layouts keys on `(id, aggregate)`.
- **Circle packing is top-down** (scale-to-fit per level) rather than d3's bottom-up radii, so
  LOD can skip invisible subtrees; sibling sizes stay exactly proportional within a level.

## Blockers

None.

## Core change requests

None required. Optional, for the lead to decide when wiring the index:

`strata-index` will implement `LayoutSource`. If the index should not depend on
`strata-layout`, move the trait into `strata-core` and re-export it from `strata-layout`:

```diff
--- a/crates/strata-core/src/lib.rs
+++ b/crates/strata-core/src/lib.rs
@@
 mod flags;
 pub mod known;
+pub mod layout_source;
 mod name;
@@
+pub use layout_source::{LayoutSource, NodeId};
```

(`layout_source.rs` = the trait and `NodeId` alias from `crates/strata-layout/src/source.rs`,
unchanged.) Otherwise `strata-index` simply adds `strata-layout = { path = "../strata-layout" }`;
the crate has no dependencies, so that costs nothing.

## Next steps

- Wire into `src-tauri`: a command that takes `{root, viewport, dpr, view, mode}` and streams
  `rects/aggregates/labels[/cushions]` over a Channel; keep a `RectLayout` per window and use
  `layout_treemap_into` to reuse allocations.
- Implement `LayoutSource` on the index (sizes in the active `SizeMode`; `color_key` from the
  active color mode).
- Label candidates for the sunburst (arc-aligned text boxes) and mind map (beside nodes).
- Mind map expand/collapse state (per-node `max_children` override) for "depth-limited and
  expandable".
- Marquee selection helper (rect query over `HitGrid`).
