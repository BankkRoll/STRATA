/**
 * Renderer loading. The treemap renderer ships in the main bundle (it is the
 * default view); sunburst and circle renderers are split into lazy chunks and
 * loaded the first time their view opens ({@link loadRenderer}).
 */
import { ViewKind } from "../lib/layout/frame";
import { RectRenderer } from "./RectRenderer";
import type { ViewRenderer } from "./renderer";

type Ctor = new (gl: WebGL2RenderingContext) => ViewRenderer;

let arcCtor: Ctor | null = null;
let circleCtor: (new (gl: WebGL2RenderingContext, mindMap: boolean) => ViewRenderer) | null = null;

/**
 * Ensures the renderer module for `kind` is loaded.
 *
 * @param kind - View kind.
 */
export async function loadRenderer(kind: ViewKind): Promise<void> {
  if (kind === ViewKind.Sunburst && !arcCtor) arcCtor = (await import("./ArcRenderer")).ArcRenderer;
  if ((kind === ViewKind.Bubbles || kind === ViewKind.MindMap) && !circleCtor) {
    circleCtor = (await import("./CircleRenderer")).CircleRenderer;
  }
}

/** Whether {@link createRenderer} can build `kind` synchronously. */
export function rendererLoaded(kind: ViewKind): boolean {
  if (kind === ViewKind.Sunburst) return arcCtor !== null;
  if (kind === ViewKind.Bubbles || kind === ViewKind.MindMap) return circleCtor !== null;
  return true;
}

/**
 * Gets the canvas's WebGL2 context and builds the renderer for `kind`
 * (whose module must be loaded, see {@link loadRenderer}).
 *
 * @param canvas - Target canvas.
 * @param kind - Layout view kind.
 * @returns The renderer, or `null` when WebGL2 is unavailable.
 * @throws {ShaderError} When a shader fails to compile.
 */
export function createRenderer(canvas: HTMLCanvasElement, kind: ViewKind): ViewRenderer | null {
  // PERF: no MSAA — rect edges are pixel-aligned and circles anti-alias in
  // the shader, and multisampling half a million instances costs fill rate.
  // An opaque canvas skips compositing the page behind it.
  const gl = canvas.getContext("webgl2", {
    antialias: false,
    alpha: false,
    depth: false,
    stencil: false,
    premultipliedAlpha: true,
    powerPreference: "high-performance",
  });
  if (!gl) return null;
  switch (kind) {
    case ViewKind.Treemap:
    case ViewKind.Icicle:
    case ViewKind.Flame:
      return new RectRenderer(gl);
    case ViewKind.Sunburst:
      if (!arcCtor) throw new Error("sunburst renderer not loaded");
      return new arcCtor(gl);
    case ViewKind.Bubbles:
    case ViewKind.MindMap:
      if (!circleCtor) throw new Error("circle renderer not loaded");
      return new circleCtor(gl, kind === ViewKind.MindMap);
  }
}
