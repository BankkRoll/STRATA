/**
 * Sunburst renderer: each 32-byte arc record is one instance, tessellated in
 * the vertex shader into a triangle strip of {@link SEGMENTS} slices from
 * `gl_VertexID`, so the CPU never builds geometry.
 */
import { BaseRenderer, COMMON_UNIFORMS, GLSL_VERTEX_COMMON } from "./BaseRenderer";
import { GLSL_COMMON, createProgram, uniforms } from "./gl";
import type { DrawState } from "./renderer";

/** Slices per sector; enough for a smooth full ring at 4K. */
const SEGMENTS = 64;

const VERTEX = /* glsl */ `#version 300 es
precision highp float;
precision highp int;
layout(location = 0) in vec4 a_geom;  // a0, a1, r0, r1
layout(location = 1) in uvec4 a_meta;
layout(location = 3) in float a_sel;
${GLSL_VERTEX_COMMON}
uniform vec2 u_center;
uniform float u_gap;
flat out uint v_key;
flat out uint v_flags;
flat out uint v_depth;
flat out float v_sel;
flat out float v_hover;
out float v_edge;
void main() {
  int seg = gl_VertexID >> 1;
  bool outer = (gl_VertexID & 1) == 1;
  float a0 = a_geom.x;
  float a1 = a_geom.y;
  float r0 = a_geom.z;
  float r1 = a_geom.w;
  // A thin gap between sectors keeps neighbours apart without a stroke pass.
  float rm = max(0.5 * (r0 + r1), 1.0);
  float ga = min(u_gap / rm, 0.25 * (a1 - a0));
  if (a1 - a0 < 6.28) { a0 += ga; a1 -= ga; }
  float r = outer ? max(r1 - u_gap, r0) : (r0 > 0.0 ? r0 + u_gap : 0.0);
  float a = mix(a0, a1, float(seg) / ${SEGMENTS}.0);
  vec2 p = u_center + r * vec2(sin(a), -cos(a));
  v_key = a_meta.y;
  v_flags = a_meta.w >> 16u;
  v_depth = a_meta.w & 0xffffu;
  v_sel = a_sel;
  v_hover = gl_InstanceID == u_hover ? 1.0 : 0.0;
  v_edge = outer ? 1.0 : 0.0;
  gl_Position = toClip(p);
}
`;

const FRAGMENT = /* glsl */ `#version 300 es
precision highp float;
precision highp int;
${GLSL_COMMON}
uniform vec3 u_accent;
flat in uint v_key;
flat in uint v_flags;
flat in uint v_depth;
flat in float v_sel;
flat in float v_hover;
in float v_edge;
out vec4 o_color;
void main() {
  vec3 rgb = baseColor(v_key);
  if ((v_flags & 4u) != 0u) rgb = hatch(rgb);
  if (v_depth == 0u) rgb = mix(rgb, u_dark ? vec3(0.13) : vec3(0.92), 0.6);
  rgb *= 1.0 - 0.03 * float(min(v_depth, 8u));
  if (v_sel > 0.5) rgb = mix(rgb, u_accent, 0.45);
  if (v_hover > 0.5) rgb = highlight(rgb, 0.25);
  o_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
`;

const UNIFORMS = [...COMMON_UNIFORMS, "u_center", "u_gap"] as const;

/** Instanced, vertex-tessellated sunburst sectors. */
export class ArcRenderer extends BaseRenderer {
  private program: WebGLProgram | null = null;
  private u: Record<(typeof UNIFORMS)[number], WebGLUniformLocation | null> | null = null;
  private vao: WebGLVertexArrayObject | null = null;

  constructor(gl: WebGL2RenderingContext) {
    super(gl);
    this.init();
  }

  protected initPrograms(): void {
    const gl = this.gl;
    this.program = createProgram(gl, VERTEX, FRAGMENT, "arc");
    this.u = uniforms(gl, this.program, UNIFORMS);
    this.vao = gl.createVertexArray();
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
    gl.bindVertexArray(null);
  }

  protected drawFrame(s: DrawState): boolean {
    const gl = this.gl;
    const f = this.frame;
    if (!f) return false;
    gl.disable(gl.BLEND);
    gl.useProgram(this.program);
    if (this.u) {
      this.setCommon(this.u, s);
      gl.uniform2f(this.u.u_center, f.centerX, f.centerY);
      gl.uniform1f(this.u.u_gap, 0.5 * s.dpr);
    }
    gl.bindVertexArray(this.vao);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, (SEGMENTS + 1) * 2, f.nodes.count);
    gl.bindVertexArray(null);
    return false;
  }

  protected disposePrograms(): void {
    this.gl.deleteProgram(this.program);
    this.gl.deleteVertexArray(this.vao);
  }
}
