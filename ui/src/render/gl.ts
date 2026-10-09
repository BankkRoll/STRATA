/**
 * Small WebGL2 helpers shared by the view renderers: shader compilation with
 * readable errors, the palette texture, and the common GLSL chunks for
 * color modes, patterns and highlights.
 */
import { PALETTE_ROWS, PALETTE_WIDTH } from "../lib/palette";

/** Raised when a shader fails to compile or link; the message has the GL log. */
export class ShaderError extends Error {
  override name = "ShaderError";
}

/**
 * Compiles and links a program.
 *
 * @param gl - Context.
 * @param vs - Vertex shader source.
 * @param fs - Fragment shader source.
 * @param label - Name used in error messages.
 * @returns The linked program.
 * @throws {ShaderError} With the info log on failure (unless the context is lost).
 */
export function createProgram(gl: WebGL2RenderingContext, vs: string, fs: string, label: string): WebGLProgram {
  const compile = (type: number, src: string) => {
    const s = gl.createShader(type);
    if (!s) throw new ShaderError(`${label}: createShader failed`);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS) && !gl.isContextLost()) {
      const log = gl.getShaderInfoLog(s) ?? "";
      gl.deleteShader(s);
      throw new ShaderError(`${label} ${type === gl.VERTEX_SHADER ? "vertex" : "fragment"} shader: ${log}`);
    }
    return s;
  };
  const v = compile(gl.VERTEX_SHADER, vs);
  const f = compile(gl.FRAGMENT_SHADER, fs);
  const p = gl.createProgram();
  gl.attachShader(p, v);
  gl.attachShader(p, f);
  gl.linkProgram(p);
  gl.deleteShader(v);
  gl.deleteShader(f);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS) && !gl.isContextLost()) {
    throw new ShaderError(`${label} link: ${gl.getProgramInfoLog(p) ?? ""}`);
  }
  return p;
}

/** Looks up uniform locations by name. */
export function uniforms<K extends string>(
  gl: WebGL2RenderingContext,
  p: WebGLProgram,
  names: readonly K[],
): Record<K, WebGLUniformLocation | null> {
  const out = {} as Record<K, WebGLUniformLocation | null>;
  for (const n of names) out[n] = gl.getUniformLocation(p, n);
  return out;
}

/**
 * Creates (or refills) the palette lookup texture (RGBA8, nearest).
 *
 * @param gl - Context.
 * @param data - Texels from `buildPaletteTexture`.
 * @param tex - Existing texture to refill.
 * @returns The texture.
 */
export function uploadPalette(gl: WebGL2RenderingContext, data: Uint8Array, tex?: WebGLTexture): WebGLTexture {
  const t = tex ?? gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, PALETTE_WIDTH, PALETTE_ROWS, 0, gl.RGBA, gl.UNSIGNED_BYTE, data);
  return t;
}

/**
 * GLSL shared by every fragment shader: palette lookup per color mode,
 * category patterns, the aggregate hatch, and the highlight treatment.
 */
export const GLSL_COMMON = /* glsl */ `
uniform highp sampler2D u_palette;
uniform int u_mode;        // 0 category, 1 safety, 2 age, 3 file type, 4 app, 5 recent
uniform bool u_patterns;   // color-blind patterns (category mode)
uniform bool u_dark;
uniform float u_pulse;     // 0..1 for the changed-recently pulse
uniform float u_dpr;

vec4 paletteColor(uint key) {
  ivec2 at;
  if (u_mode == 0) at = ivec2(int(key & 15u), 0);
  else if (u_mode == 1) at = ivec2(int((key >> 4u) & 7u), 1);
  else if (u_mode == 2) at = ivec2(int((key >> 7u) & 31u), 2);
  else if (u_mode == 3) at = ivec2(int((key >> 12u) & 255u), 3);
  else if (u_mode == 4) at = ivec2(int((key >> 20u) & 1023u), 4);
  else at = ivec2(int((key >> 30u) & 1u), 5);
  return texelFetch(u_palette, at, 0);
}

vec3 baseColor(uint key) {
  vec4 c = paletteColor(key);
  vec3 rgb = c.rgb;
  if (u_mode == 5 && ((key >> 30u) & 1u) == 1u) {
    rgb = mix(rgb, vec3(1.0), 0.45 * u_pulse);
  }
  if (u_mode == 0 && u_patterns) {
    int pat = int(c.a * 255.0 + 0.5);
    vec2 p = gl_FragCoord.xy / max(u_dpr, 1.0);
    float on = 0.0;
    if (pat == 1) on = step(mod(p.x + p.y, 8.0), 2.0);
    else if (pat == 2) on = step(mod(p.x - p.y, 8.0), 2.0);
    else if (pat == 3) on = step(mod(p.y, 6.0), 1.5);
    else if (pat == 4) on = step(mod(p.x, 6.0), 1.5);
    else if (pat == 5) on = step(length(mod(p, 6.0) - 3.0), 1.3);
    else if (pat == 6) on = max(step(mod(p.x + p.y, 8.0), 1.5), step(mod(p.x - p.y, 8.0), 1.5));
    else if (pat == 7) on = max(step(mod(p.x, 7.0), 1.2), step(mod(p.y, 7.0), 1.2));
    vec3 ink = u_dark ? vec3(0.0) : vec3(1.0);
    rgb = mix(rgb, ink, on * 0.38);
  }
  return rgb;
}

vec3 hatch(vec3 rgb) {
  vec2 p = gl_FragCoord.xy / max(u_dpr, 1.0);
  float on = step(mod(p.x + p.y, 6.0), 2.0);
  vec3 bg = u_dark ? vec3(0.12) : vec3(0.93);
  return mix(mix(rgb, bg, 0.55), rgb, on * 0.6);
}

vec3 highlight(vec3 rgb, float amount) {
  return mix(rgb, u_dark ? vec3(1.0) : vec3(1.0), amount);
}
`;
