/**
 * Static SVG rendering of a layout frame for browsers without WebGL2.
 *
 * Draws the same node buffer the GPU renderers read, with the same palette
 * and the same directory tint, so the fallback looks like the real view.
 * Labels, hover and picking still come from the controller (they are CPU
 * code), so only the fill is replaced.
 */
import { NodeFlag, ViewKind, type LayoutFrame } from "@ui/lib/layout/frame";
import { rgbFor, type ColorMode, type Rgb } from "@ui/lib/palette";

const SVG = "http://www.w3.org/2000/svg";

function mix(a: Rgb, b: Rgb, t: number): string {
  const c = (k: 0 | 1 | 2) => Math.round(a[k] + (b[k] - a[k]) * t);
  return `rgb(${c(0)} ${c(1)} ${c(2)})`;
}

function sector(cx: number, cy: number, a0: number, a1: number, r0: number, r1: number): string {
  const pt = (a: number, r: number) => `${(cx + r * Math.sin(a)).toFixed(2)} ${(cy - r * Math.cos(a)).toFixed(2)}`;
  if (a1 - a0 >= Math.PI * 2 - 1e-4) {
    const mid = a0 + Math.PI;
    const ring = (r: number) => `M${pt(a0, r)}A${r} ${r} 0 1 1 ${pt(mid, r)}A${r} ${r} 0 1 1 ${pt(a0, r)}Z`;
    return r0 > 0 ? `${ring(r1)}${ring(r0)}` : ring(r1);
  }
  const large = a1 - a0 > Math.PI ? 1 : 0;
  const outer = `M${pt(a0, r1)}A${r1} ${r1} 0 ${large} 1 ${pt(a1, r1)}`;
  const inner = r0 > 0 ? `L${pt(a1, r0)}A${r0} ${r0} 0 ${large} 0 ${pt(a0, r0)}` : `L${cx} ${cy}`;
  return `${outer}${inner}Z`;
}

/**
 * Renders `frame` into `svg` (replacing its content). The SVG's viewBox is
 * the frame's own coordinate space; `preserveAspectRatio` letterboxes it the
 * same way the demo stream fits frames to the canvas.
 *
 * @param svg - Target element.
 * @param frame - Decoded layout frame.
 * @param mode - Active color mode.
 * @param dark - Dark palette.
 */
export function drawFallback(svg: SVGSVGElement, frame: LayoutFrame, mode: ColorMode, dark: boolean): void {
  const n = frame.nodes;
  const bg: Rgb = dark ? [33, 33, 33] : [230, 230, 230];
  svg.setAttribute("viewBox", `0 0 ${frame.width} ${frame.height}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  const parts: string[] = [];
  const edge = dark ? "rgb(0 0 0 / 0.45)" : "rgb(0 0 0 / 0.22)";
  for (let i = 0; i < n.count; i++) {
    const flags = n.flags(i);
    const isDir = (flags & NodeFlag.DIR) !== 0;
    const isAgg = (flags & NodeFlag.AGGREGATE) !== 0;
    const rgb = rgbFor(mode, n.colorKey(i), dark);
    const fill = isAgg ? mix(rgb, bg, 0.4) : isDir ? mix(rgb, bg, dark ? 0.6 : 0.5) : `rgb(${rgb[0]} ${rgb[1]} ${rgb[2]})`;
    const pattern = isAgg ? ` fill-opacity="0.85"` : "";
    const [a, b, c, d] = [n.geom(i, 0), n.geom(i, 1), n.geom(i, 2), n.geom(i, 3)];
    switch (frame.view) {
      case ViewKind.Treemap:
      case ViewKind.Icicle:
      case ViewKind.Flame: {
        if (c <= 0 || d <= 0) continue;
        const stroke = !isDir && Math.min(c, d) >= 4 ? ` stroke="${edge}" stroke-width="1"` : "";
        parts.push(`<rect x="${a.toFixed(1)}" y="${b.toFixed(1)}" width="${c.toFixed(1)}" height="${d.toFixed(1)}" fill="${fill}"${pattern}${stroke}/>`);
        break;
      }
      case ViewKind.Sunburst: {
        const depthTint = n.depth(i) === 0 ? mix(rgb, bg, 0.6) : fill;
        parts.push(`<path d="${sector(frame.centerX, frame.centerY, a, b, c, d)}" fill="${depthTint}" fill-rule="evenodd" stroke="${dark ? "#1b1b1b" : "#f1f1f1"}" stroke-width="1"/>`);
        break;
      }
      case ViewKind.Bubbles:
      case ViewKind.MindMap: {
        if (c <= 0) continue;
        const dirFill = isDir ? mix(rgb, bg, dark ? 0.82 : 0.78) : fill;
        parts.push(`<circle cx="${a.toFixed(1)}" cy="${b.toFixed(1)}" r="${c.toFixed(1)}" fill="${dirFill}" stroke="${isDir ? mix(rgb, bg, 0.2) : edge}" stroke-width="1"/>`);
        break;
      }
    }
  }
  svg.innerHTML = parts.join("");
  if (!svg.getAttribute("xmlns")) svg.setAttribute("xmlns", SVG);
}
