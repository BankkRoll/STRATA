# strata-layout

Layout engines for the visual views. Each reads a sized tree through `LayoutSource`, computes
geometry in device pixels, and writes compact little-endian records into byte buffers
(`RecordBuf`) that travel to the frontend over a Tauri Channel and are uploaded as WebGL2
instance data with no further serialization. It lays out a 100k-entry subtree in about 3 ms.
No runtime dependencies, `#![forbid(unsafe_code)]`.

## Responsibilities

- `layout_treemap`: nested squarified treemap with padding, header strips, small-item
  aggregation, depth limit and viewport culling (`ViewTransform`); optional cushion shading.
- `layout_icicle`, `layout_sunburst`, `layout_pack` (circle packing), `layout_mindmap`.
- Picking: `RectLayout::pick`, `ArcLayout::pick`, `CircleLayout::pick`, `HitGrid`.
- `transition`: matches two layouts by id for animated drill-down and live size changes.

Output is deterministic: the same input and config give bit-identical bytes, whatever order the
source lists children in. Byte formats are documented in the `buffer` module.

## Test

```powershell
cargo test -p strata-layout
cargo bench -p strata-layout --bench layout
pnpm --dir ui fixtures      # re-export the frames the UI tests use
```
