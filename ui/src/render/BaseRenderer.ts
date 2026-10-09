/**
 * Shared plumbing for the WebGL2 view renderers: the node buffer upload,
 * the per-instance selection byte, the palette texture and the common
 * uniforms. Subclasses add their programs and draw calls.
 */
import type { LayoutFrame } from "../lib/layout/frame";
import { NodeFlag } from "../lib/layout/frame";
import { buildPaletteTexture } from "../lib/palette";
import { uploadPalette } from "./gl";
import type { DrawState, ViewRenderer } from "./renderer";

/** Uniform names every program declares (via `GLSL_COMMON` and the vertex stage). */
export const COMMON_UNIFORMS = [
  "u_palette",
  "u_mode",
  "u_patterns",
  "u_dark",
  "u_pulse",
  "u_dpr",
  "u_viewport",
  "u_view",
  "u_hover",
  "u_accent",
] as const;

type CommonUniform = (typeof COMMON_UNIFORMS)[number];

/** Base class: owns the node buffer, selection buffer and palette. */
export abstract class BaseRenderer implements ViewRenderer {
  protected frame: LayoutFrame | null = null;
  protected nodeBuf: WebGLBuffer | null = null;
  protected selBuf: WebGLBuffer | null = null;
  protected selData = new Uint8Array(0);
  /** Whether the latest `setFrame` asked for its transition to play. */
  protected animateRequested = false;
  private palette: WebGLTexture | null = null;
  private dark = false;
  private selected: ReadonlySet<number> = new Set();

  /** @param gl - A WebGL2 context the renderer owns exclusively. */
  constructor(protected readonly gl: WebGL2RenderingContext) {}

  /** Creates programs and VAOs; called at construction and after context restore. */
  protected abstract initPrograms(): void;
  /** Rebinds vertex attributes after buffers change. */
  protected abstract bindAttributes(): void;
  /** Issues the draw calls. */
  protected abstract drawFrame(state: DrawState): boolean;
  /** Deletes subclass resources. */
  protected abstract disposePrograms(): void;

  /** Creates everything; subclasses call this at the end of their constructor. */
  protected init(): void {
    const gl = this.gl;
    this.palette = uploadPalette(gl, buildPaletteTexture(this.dark));
    this.nodeBuf = gl.createBuffer();
    this.selBuf = gl.createBuffer();
    this.initPrograms();
  }

  setFrame(frame: LayoutFrame, animate: boolean): void {
    this.frame = frame;
    this.animateRequested = animate;
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.nodeBuf);
    gl.bufferData(gl.ARRAY_BUFFER, frame.nodes.bytes, gl.STATIC_DRAW);
    this.selData = new Uint8Array(frame.nodes.count);
    this.fillSelection();
    this.bindAttributes();
  }

  setSelection(ids: ReadonlySet<number>): void {
    this.selected = ids;
    this.fillSelection();
  }

  private fillSelection(): void {
    const f = this.frame;
    if (!f) return;
    const n = f.nodes.count;
    const sel = this.selData;
    const u32 = f.nodes.u32;
    const u16 = f.nodes.u16;
    const ids = this.selected;
    if (ids.size === 0) {
      sel.fill(0);
    } else {
      for (let i = 0; i < n; i++) {
        const selectable = ((u16[i * 16 + 15] ?? 0) & NodeFlag.SELECTABLE) !== 0;
        sel[i] = selectable && ids.has(u32[i * 8 + 4] ?? -1) ? 1 : 0;
      }
    }
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.selBuf);
    gl.bufferData(gl.ARRAY_BUFFER, sel, gl.DYNAMIC_DRAW);
  }

  setTheme(dark: boolean): void {
    if (dark === this.dark) return;
    this.dark = dark;
    if (this.palette) uploadPalette(this.gl, buildPaletteTexture(dark), this.palette);
  }

  /** Sets the uniforms every program shares. */
  protected setCommon(u: Record<CommonUniform, WebGLUniformLocation | null>, s: DrawState): void {
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.palette);
    gl.uniform1i(u.u_palette, 0);
    gl.uniform1i(u.u_mode, s.colorMode);
    gl.uniform1i(u.u_patterns, s.patterns ? 1 : 0);
    gl.uniform1i(u.u_dark, s.dark ? 1 : 0);
    gl.uniform1f(u.u_pulse, s.pulse);
    gl.uniform1f(u.u_dpr, s.dpr);
    gl.uniform2f(u.u_viewport, s.width, s.height);
    gl.uniform3f(u.u_view, s.view.scale, s.view.tx, s.view.ty);
    gl.uniform1i(u.u_hover, s.hover);
    gl.uniform3f(u.u_accent, s.accent[0], s.accent[1], s.accent[2]);
  }

  draw(state: DrawState): boolean {
    const gl = this.gl;
    if (gl.isContextLost()) return false;
    gl.viewport(0, 0, state.width, state.height);
    gl.clearColor(state.background[0], state.background[1], state.background[2], 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (!this.frame || this.frame.nodes.count === 0) return false;
    return this.drawFrame(state);
  }

  restore(): void {
    this.init();
    const f = this.frame;
    if (f) this.setFrame(f, false);
  }

  dispose(): void {
    const gl = this.gl;
    this.disposePrograms();
    gl.deleteBuffer(this.nodeBuf);
    gl.deleteBuffer(this.selBuf);
    gl.deleteTexture(this.palette);
    this.frame = null;
  }
}

/** Vertex-stage declarations shared by the instanced programs. */
export const GLSL_VERTEX_COMMON = /* glsl */ `
uniform vec2 u_viewport;
uniform vec3 u_view;   // scale, tx, ty applied to frame coordinates
uniform int u_hover;
vec4 toClip(vec2 framePx) {
  vec2 s = framePx * u_view.x + u_view.yz;
  vec2 ndc = s / u_viewport * 2.0 - 1.0;
  return vec4(ndc.x, -ndc.y, 0.0, 1.0);
}
`;
