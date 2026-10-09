/**
 * Bubbles (circle packing) and mind-map renderer: one instanced quad per
 * 32-byte circle record, shaded as an anti-aliased signed-distance circle in
 * the fragment shader. The mind map first draws parent→child edges as
 * instanced line quads.
 */
import { NO_INDEX, type LayoutFrame } from "../lib/layout/frame";
import { BaseRenderer, COMMON_UNIFORMS, GLSL_VERTEX_COMMON } from "./BaseRenderer";
import { GLSL_COMMON, createProgram, uniforms } from "./gl";
import type { DrawState } from "./renderer";

const CIRCLE_VERTEX = /* glsl */ `#version 300 es
precision highp float;
precision highp int;
layout(location = 0) in vec4 a_geom;  // cx, cy, r, aux
layout(location = 1) in uvec4 a_meta;
layout(location = 3) in float a_sel;
${GLSL_VERTEX_COMMON}
out vec2 v_local;
flat out float v_r;
flat out uint v_key;
flat out uint v_flags;
flat out float v_sel;
flat out float v_hover;
void main() {
  vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1)) * 2.0 - 1.0;
  float r = a_geom.z;
  // One extra pixel of quad so the anti-aliased edge is not clipped.
  float pad = 1.5 / max(u_view.x, 1e-6);
  vec2 p = a_geom.xy + corner * (r + pad);
  v_local = corner * (r + pad) * u_view.x;
  v_r = r * u_view.x;
  v_key = a_meta.y;
  v_flags = a_meta.w >> 16u;
  v_sel = a_sel;
  v_hover = gl_InstanceID == u_hover ? 1.0 : 0.0;
  gl_Position = toClip(p);
}
`;

const CIRCLE_FRAGMENT = /* glsl */ `#version 300 es
precision highp float;
precision highp int;
${GLSL_COMMON}
uniform vec3 u_accent;
uniform bool u_hollowDirs;
in vec2 v_local;
flat in float v_r;
flat in uint v_key;
flat in uint v_flags;
flat in float v_sel;
flat in float v_hover;
out vec4 o_color;
void main() {
  float dist = length(v_local);
  float cover = clamp(v_r - dist + 0.5, 0.0, 1.0);
  if (cover <= 0.0) discard;
  bool isDir = (v_flags & 1u) != 0u;
  vec3 rgb = baseColor(v_key);
  if ((v_flags & 4u) != 0u) rgb = hatch(rgb);
  float ringW = max(u_dpr, 1.0);
  bool onRing = dist > v_r - ringW;
  if (isDir && u_hollowDirs) {
    vec3 bg = u_dark ? vec3(0.13) : vec3(0.95);
    rgb = onRing ? mix(rgb, bg, 0.2) : mix(rgb, bg, u_dark ? 0.82 : 0.78);
  } else if (onRing && v_r > 4.0) {
    rgb *= u_dark ? 0.6 : 0.8;
  }
  if (v_sel > 0.5) rgb = dist > v_r - 2.5 * ringW ? u_accent : mix(rgb, u_accent, 0.2);
  if (v_hover > 0.5) rgb = highlight(rgb, 0.22);
  o_color = vec4(clamp(rgb, 0.0, 1.0) * cover, cover);
}
`;

const LINE_VERTEX = /* glsl */ `#version 300 es
precision highp float;
layout(location = 6) in vec4 a_line;  // x0, y0, x1, y1
uniform vec2 u_viewport;
uniform vec3 u_view;
uniform float u_width;
vec4 toClip(vec2 framePx) {
  vec2 s = framePx * u_view.x + u_view.yz;
  vec2 ndc = s / u_viewport * 2.0 - 1.0;
  return vec4(ndc.x, -ndc.y, 0.0, 1.0);
}
void main() {
  float along = float(gl_VertexID & 1);
  float side = float(gl_VertexID >> 1) * 2.0 - 1.0;
  vec2 a = a_line.xy;
  vec2 b = a_line.zw;
  vec2 dir = b - a;
  float len = max(length(dir), 1e-6);
  vec2 n = vec2(-dir.y, dir.x) / len;
  vec2 p = mix(a, b, along) + n * side * (0.5 * u_width / max(u_view.x, 1e-6));
  gl_Position = toClip(p);
}
`;

const LINE_FRAGMENT = /* glsl */ `#version 300 es
precision highp float;
uniform vec3 u_lineColor;
out vec4 o_color;
void main() { o_color = vec4(u_lineColor, 1.0); }
`;

const UNIFORMS = [...COMMON_UNIFORMS, "u_hollowDirs"] as const;
const LINE_UNIFORMS = ["u_viewport", "u_view", "u_width", "u_lineColor"] as const;

/** Instanced SDF circles, plus edges for the mind map. */
export class CircleRenderer extends BaseRenderer {
  private program: WebGLProgram | null = null;
  private lineProgram: WebGLProgram | null = null;
  private u: Record<(typeof UNIFORMS)[number], WebGLUniformLocation | null> | null = null;
  private lu: Record<(typeof LINE_UNIFORMS)[number], WebGLUniformLocation | null> | null = null;
  private vao: WebGLVertexArrayObject | null = null;
  private lineVao: WebGLVertexArrayObject | null = null;
  private lineBuf: WebGLBuffer | null = null;
  private lineCount = 0;

  /**
   * @param gl - Context.
   * @param mindMap - Draw edges and solid nodes (mind map) instead of nested
   *   hollow directories (bubbles).
   */
  constructor(
    gl: WebGL2RenderingContext,
    private readonly mindMap: boolean,
  ) {
    super(gl);
    this.init();
  }

  protected initPrograms(): void {
    const gl = this.gl;
    this.program = createProgram(gl, CIRCLE_VERTEX, CIRCLE_FRAGMENT, "circle");
    this.u = uniforms(gl, this.program, UNIFORMS);
    this.vao = gl.createVertexArray();
    if (this.mindMap) {
      this.lineProgram = createProgram(gl, LINE_VERTEX, LINE_FRAGMENT, "mindmap-edge");
      this.lu = uniforms(gl, this.lineProgram, LINE_UNIFORMS);
      this.lineVao = gl.createVertexArray();
      this.lineBuf = gl.createBuffer();
    }
  }

  override setFrame(frame: LayoutFrame, animate: boolean): void {
    if (this.mindMap) {
      // Edges need both endpoints; the mind map is small (top-K per level),
      // so building them on the CPU is negligible.
      const n = frame.nodes.count;
      const lines = new Float32Array(Math.max(0, n - 1) * 4);
      let k = 0;
      for (let i = 0; i < n; i++) {
        const p = frame.nodes.parent(i);
        if (p === NO_INDEX || p >= n) continue;
        lines[k++] = frame.nodes.geom(p, 0);
        lines[k++] = frame.nodes.geom(p, 1);
        lines[k++] = frame.nodes.geom(i, 0);
        lines[k++] = frame.nodes.geom(i, 1);
      }
      this.lineCount = k / 4;
      const gl = this.gl;
      gl.bindBuffer(gl.ARRAY_BUFFER, this.lineBuf);
      gl.bufferData(gl.ARRAY_BUFFER, lines.subarray(0, k), gl.STATIC_DRAW);
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
    gl.bindBuffer(gl.ARRAY_BUFFER, this.selBuf);
    gl.enableVertexAttribArray(3);
    gl.vertexAttribPointer(3, 1, gl.UNSIGNED_BYTE, true, 1, 0);
    gl.vertexAttribDivisor(3, 1);
    if (this.mindMap) {
      gl.bindVertexArray(this.lineVao);
      gl.bindBuffer(gl.ARRAY_BUFFER, this.lineBuf);
      gl.enableVertexAttribArray(6);
      gl.vertexAttribPointer(6, 4, gl.FLOAT, false, 16, 0);
      gl.vertexAttribDivisor(6, 1);
    }
    gl.bindVertexArray(null);
  }

  protected drawFrame(s: DrawState): boolean {
    const gl = this.gl;
    const f = this.frame;
    if (!f) return false;
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    if (this.mindMap && this.lineCount > 0 && this.lu) {
      gl.useProgram(this.lineProgram);
      gl.uniform2f(this.lu.u_viewport, s.width, s.height);
      gl.uniform3f(this.lu.u_view, s.view.scale, s.view.tx, s.view.ty);
      gl.uniform1f(this.lu.u_width, 1.5 * s.dpr);
      if (s.dark) gl.uniform3f(this.lu.u_lineColor, 0.42, 0.44, 0.48);
      else gl.uniform3f(this.lu.u_lineColor, 0.62, 0.64, 0.68);
      gl.bindVertexArray(this.lineVao);
      gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, this.lineCount);
    }
    gl.useProgram(this.program);
    if (this.u) {
      this.setCommon(this.u, s);
      gl.uniform1i(this.u.u_hollowDirs, this.mindMap ? 0 : 1);
    }
    gl.bindVertexArray(this.vao);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, f.nodes.count);
    gl.bindVertexArray(null);
    return false;
  }

  protected disposePrograms(): void {
    const gl = this.gl;
    gl.deleteProgram(this.program);
    gl.deleteProgram(this.lineProgram);
    gl.deleteVertexArray(this.vao);
    gl.deleteVertexArray(this.lineVao);
    gl.deleteBuffer(this.lineBuf);
  }
}
