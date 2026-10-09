/**
 * Treemap and icicle/flame renderer: one instanced quad per 32-byte rect
 * record, read straight from the uploaded layout bytes (`vec4` geometry at
 * offset 0, `uvec4` meta at offset 16 via `vertexAttribIPointer`).
 *
 * - Flat style, or cushion style from the parallel cushion section using
 *   the shading formula in `docs/tracks/layout.md`.
 * - Directory frames are tinted toward the background so nesting reads;
 *   aggregates are hatched; leaves get a 1-device-pixel border.
 * - Hover and selection are uniforms / a per-instance byte, so neither
 *   re-uploads geometry.
 * - Drill transitions: the transition section is uploaded once and tweened
 *   entirely in the vertex shader from a time uniform.
 */
import { NO_INDEX, type LayoutFrame } from "../lib/layout/frame";
import { BaseRenderer, COMMON_UNIFORMS, GLSL_VERTEX_COMMON } from "./BaseRenderer";
import { GLSL_COMMON, createProgram, uniforms } from "./gl";
import { TRANSITION_MS, easeInOut, type DrawState } from "./renderer";

const vertex = (transition: boolean) => /* glsl */ `#version 300 es
precision highp float;
precision highp int;
#define TRANSITION ${transition ? 1 : 0}
layout(location = 0) in vec4 a_geom;
layout(location = 1) in uvec4 a_meta;
layout(location = 2) in vec4 a_cushion;
layout(location = 3) in float a_sel;
layout(location = 4) in vec4 a_to;
layout(location = 5) in uvec2 a_extra;
${GLSL_VERTEX_COMMON}
uniform float u_t;
out vec2 v_layout;
out vec2 v_local;
flat out vec2 v_size;
flat out uint v_key;
flat out uint v_flags;
flat out uint v_depth;
flat out float v_sel;
flat out float v_hover;
flat out float v_alpha;
flat out vec4 v_cushion;
void main() {
  vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
#if TRANSITION
  vec4 g = mix(a_geom, a_to, u_t);
  uint kind = a_meta.w & 0xffffu;
  v_flags = a_meta.w >> 16u;
  v_key = a_extra.x;
  v_depth = a_extra.y;
  v_alpha = kind == 1u ? u_t : (kind == 2u ? 1.0 - u_t : 1.0);
  v_sel = 0.0;
  v_hover = 0.0;
  v_cushion = vec4(0.0);
#else
  vec4 g = a_geom;
  v_key = a_meta.y;
  v_flags = a_meta.w >> 16u;
  v_depth = a_meta.w & 0xffffu;
  v_alpha = 1.0;
  v_sel = a_sel;
  v_hover = gl_InstanceID == u_hover ? 1.0 : 0.0;
  v_cushion = a_cushion;
#endif
  vec2 p = g.xy + corner * g.zw;
  v_layout = p;
  v_size = g.zw * u_view.x;
  v_local = corner * v_size;
  gl_Position = toClip(p);
}
`;

const FRAGMENT = /* glsl */ `#version 300 es
precision highp float;
precision highp int;
${GLSL_COMMON}
uniform bool u_cushion;
uniform vec3 u_accent;
in vec2 v_layout;
in vec2 v_local;
flat in vec2 v_size;
flat in uint v_key;
flat in uint v_flags;
flat in uint v_depth;
flat in float v_sel;
flat in float v_hover;
flat in float v_alpha;
flat in vec4 v_cushion;
out vec4 o_color;
void main() {
  bool isDir = (v_flags & 1u) != 0u;
  bool isAgg = (v_flags & 4u) != 0u;
  vec3 rgb = baseColor(v_key);
  if (isAgg) {
    rgb = hatch(rgb);
  } else if (isDir) {
    vec3 bg = u_dark ? vec3(0.13) : vec3(0.90);
    rgb = mix(rgb, bg, u_dark ? 0.6 : 0.5);
  }
  // Deeper levels shift slightly so adjacent nesting levels separate.
  float d = float(min(v_depth, 12u));
  rgb = u_dark ? rgb * (1.0 - 0.015 * d) : rgb * (1.0 - 0.01 * d);
  if (u_cushion && !isAgg) {
    float nx = -(2.0 * v_cushion.x * v_layout.x + v_cushion.z);
    float ny = -(2.0 * v_cushion.y * v_layout.y + v_cushion.w);
    vec3 L = normalize(vec3(-1.0, -1.0, 10.0));
    float cosa = (nx * L.x + ny * L.y + L.z) / sqrt(nx * nx + ny * ny + 1.0);
    float I = 0.15 + 0.85 * max(0.0, cosa);
    rgb *= I * 1.12;
  }
  float edge = min(min(v_local.x, v_local.y), min(v_size.x - v_local.x, v_size.y - v_local.y));
  if (!isDir && min(v_size.x, v_size.y) >= 4.0 && edge < 1.0) {
    rgb *= u_dark ? 0.55 : 0.78;
  }
  float ring = 2.0 * max(u_dpr, 1.0);
  if (v_sel > 0.5) {
    rgb = edge < ring ? u_accent : mix(rgb, u_accent, 0.2);
  }
  if (v_hover > 0.5) {
    rgb = highlight(rgb, 0.2);
    if (edge < ring * 0.75) rgb = u_dark ? vec3(1.0) : vec3(0.08);
  }
  o_color = vec4(clamp(rgb, 0.0, 1.0) * v_alpha, v_alpha);
}
`;

const UNIFORMS = [...COMMON_UNIFORMS, "u_cushion", "u_t"] as const;

/** Instanced rect renderer for treemap, icicle and flame. */
export class RectRenderer extends BaseRenderer {
  private main: WebGLProgram | null = null;
  private trans: WebGLProgram | null = null;
  private uMain: Record<(typeof UNIFORMS)[number], WebGLUniformLocation | null> | null = null;
  private uTrans: Record<(typeof UNIFORMS)[number], WebGLUniformLocation | null> | null = null;
  private vao: WebGLVertexArrayObject | null = null;
  private tvao: WebGLVertexArrayObject | null = null;
  private cushionBuf: WebGLBuffer | null = null;
  private transBuf: WebGLBuffer | null = null;
  private extraBuf: WebGLBuffer | null = null;
  private hasCushions = false;
  private transitionCount = 0;
  private transitionStart: number | null = null;
  private pendingTransition = false;

  constructor(gl: WebGL2RenderingContext) {
    super(gl);
    this.init();
  }

  protected initPrograms(): void {
    const gl = this.gl;
    this.main = createProgram(gl, vertex(false), FRAGMENT, "rect");
    this.trans = createProgram(gl, vertex(true), FRAGMENT, "rect-transition");
    this.uMain = uniforms(gl, this.main, UNIFORMS);
    this.uTrans = uniforms(gl, this.trans, UNIFORMS);
    this.vao = gl.createVertexArray();
    this.tvao = gl.createVertexArray();
    this.cushionBuf = gl.createBuffer();
    this.transBuf = gl.createBuffer();
    this.extraBuf = gl.createBuffer();
  }

  override setFrame(frame: LayoutFrame, animate: boolean): void {
    const gl = this.gl;
    this.hasCushions = frame.cushions !== null;
    if (frame.cushions) {
      gl.bindBuffer(gl.ARRAY_BUFFER, this.cushionBuf);
      gl.bufferData(gl.ARRAY_BUFFER, frame.cushions, gl.STATIC_DRAW);
    }
    const old = this.frame;
    const t = frame.transitions;
    this.transitionCount = 0;
    if (animate && t && old) {
      gl.bindBuffer(gl.ARRAY_BUFFER, this.transBuf);
      gl.bufferData(gl.ARRAY_BUFFER, t.bytes, gl.STATIC_DRAW);
      // Transition records carry no color key or depth; look them up once
      // in whichever frame still has the record.
      const extra = new Uint32Array(t.count * 2);
      for (let i = 0; i < t.count; i++) {
        const ni = t.newIndex(i);
        const src = ni !== NO_INDEX ? frame.nodes : old.nodes;
        const idx = ni !== NO_INDEX ? ni : t.oldIndex(i);
        if (idx < src.count) {
          extra[i * 2] = src.colorKey(idx);
          extra[i * 2 + 1] = src.depth(idx);
        }
      }
      gl.bindBuffer(gl.ARRAY_BUFFER, this.extraBuf);
      gl.bufferData(gl.ARRAY_BUFFER, extra, gl.STATIC_DRAW);
      this.transitionCount = t.count;
      this.pendingTransition = true;
      this.transitionStart = null;
    }
    super.setFrame(frame, animate);
  }

  protected bindAttributes(): void {
    const gl = this.gl;
    gl.bindVertexArray(this.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.nodeBuf);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 32, 0);
    gl.vertexAttribDivisor(0, 1);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribIPointer(1, 4, gl.UNSIGNED_INT, 32, 16);
    gl.vertexAttribDivisor(1, 1);
    if (this.hasCushions) {
      gl.bindBuffer(gl.ARRAY_BUFFER, this.cushionBuf);
      gl.enableVertexAttribArray(2);
      gl.vertexAttribPointer(2, 4, gl.FLOAT, false, 16, 0);
      gl.vertexAttribDivisor(2, 1);
    } else {
      gl.disableVertexAttribArray(2);
      gl.vertexAttrib4f(2, 0, 0, 0, 0);
    }
    gl.bindBuffer(gl.ARRAY_BUFFER, this.selBuf);
    gl.enableVertexAttribArray(3);
    gl.vertexAttribPointer(3, 1, gl.UNSIGNED_BYTE, true, 1, 0);
    gl.vertexAttribDivisor(3, 1);

    gl.bindVertexArray(this.tvao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.transBuf);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 48, 0);
    gl.vertexAttribDivisor(0, 1);
    gl.enableVertexAttribArray(4);
    gl.vertexAttribPointer(4, 4, gl.FLOAT, false, 48, 16);
    gl.vertexAttribDivisor(4, 1);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribIPointer(1, 4, gl.UNSIGNED_INT, 48, 32);
    gl.vertexAttribDivisor(1, 1);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.extraBuf);
    gl.enableVertexAttribArray(5);
    gl.vertexAttribIPointer(5, 2, gl.UNSIGNED_INT, 8, 0);
    gl.vertexAttribDivisor(5, 1);
    gl.disableVertexAttribArray(2);
    gl.vertexAttrib4f(2, 0, 0, 0, 0);
    gl.disableVertexAttribArray(3);
    gl.vertexAttrib1f(3, 0);
    gl.bindVertexArray(null);
  }

  protected drawFrame(s: DrawState): boolean {
    const gl = this.gl;
    const frame = this.frame;
    if (!frame) return false;
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);

    if (this.pendingTransition && this.transitionCount > 0 && !s.reducedMotion) {
      this.transitionStart ??= s.now;
      const raw = (s.now - this.transitionStart) / TRANSITION_MS;
      if (raw < 1) {
        gl.useProgram(this.trans);
        if (this.uTrans) {
          this.setCommon(this.uTrans, { ...s, hover: -1 });
          gl.uniform1i(this.uTrans.u_cushion, 0);
          gl.uniform1f(this.uTrans.u_t, easeInOut(raw));
        }
        gl.bindVertexArray(this.tvao);
        gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, this.transitionCount);
        gl.bindVertexArray(null);
        return true;
      }
    }
    this.pendingTransition = false;

    gl.useProgram(this.main);
    if (this.uMain) {
      this.setCommon(this.uMain, s);
      gl.uniform1i(this.uMain.u_cushion, s.cushion && this.hasCushions ? 1 : 0);
      gl.uniform1f(this.uMain.u_t, 1);
    }
    gl.bindVertexArray(this.vao);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, frame.nodes.count);
    gl.bindVertexArray(null);
    return false;
  }

  /** Whether a drill transition is still playing (labels and picking wait for it). */
  get animating(): boolean {
    return this.pendingTransition;
  }

  protected disposePrograms(): void {
    const gl = this.gl;
    gl.deleteProgram(this.main);
    gl.deleteProgram(this.trans);
    gl.deleteVertexArray(this.vao);
    gl.deleteVertexArray(this.tvao);
    gl.deleteBuffer(this.cushionBuf);
    gl.deleteBuffer(this.transBuf);
    gl.deleteBuffer(this.extraBuf);
  }
}
