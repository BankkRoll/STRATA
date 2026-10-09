/**
 * Imperative controller for one visual view: canvas sizing, layout requests,
 * GPU rendering on demand, picking, labels and pointer/keyboard interaction.
 *
 * React mounts it once and feeds it settings through {@link update}; nothing
 * here re-renders React per frame or per pointer move. Rendering happens in
 * `requestAnimationFrame` only when something changed (`invalidate`) or
 * while an animation runs.
 *
 * Responsibilities:
 * - DPR-aware sizing (`devicePixelContentBoxSize`, `matchMedia` resolution
 *   queries) with a relayout on size and DPR changes.
 * - Layout requests via the {@link LayoutStream}; frames go to the renderer,
 *   a fresh {@link Picker} and the label overlay.
 * - Hover (pick per pointer move, < 1 ms), click/Ctrl+click selection,
 *   double-click drill, mouse-back, marquee drag, middle-drag pan, wheel
 *   zoom about the cursor, context menu, keyboard navigation.
 * - WebGL context loss and restore.
 */
import type { EntryInfoProvider } from "../lib/entries";
import type { SizeUnits } from "../lib/format";
import { IDENTITY_TRANSFORM, NO_INDEX, NodeFlag, ViewKind, type LayoutFrame, type ViewTransform } from "../lib/layout/frame";
import { isNavKey, navigate, recordOf } from "../lib/layout/navigate";
import { ancestorIds, createPicker, marqueeSelect, type Picker } from "../lib/layout/pick";
import type { LayoutRequest, LayoutStream } from "../lib/layout/stream";
import { clampTransform, invertPoint, panBy, relativeTransform, sameTransform, zoomAbout } from "../lib/layout/transform";
import { COLOR_MODE_INDEX, type ColorMode } from "../lib/palette";
import type { SizeMode, TreemapStyle, ViewFilters, VisualView } from "../lib/types";
import { hoverStore } from "../store/hover";
import { LabelOverlay } from "./labels";
import type { DrawState, ViewRenderer } from "./renderer";

/** Everything the controller needs to know from the app state. */
export interface ViewSettings {
  volumeId: string;
  root: number;
  /**
   * Changes whenever the volume's index changes (a new scan, a rescan or
   * live updates); a new value re-requests the layout.
   */
  revision?: string;
  view: VisualView;
  sizeMode: SizeMode;
  style: TreemapStyle;
  filters: ViewFilters;
  colorMode: ColorMode;
  patterns: boolean;
  dark: boolean;
  units: SizeUnits;
  reducedMotion: boolean;
  selection: readonly number[];
  primary: number | null;
}

/** A right-click request for the context menu. */
export interface ContextMenuRequest {
  ids: number[];
  clientX: number;
  clientY: number;
}

/** Callbacks into the app. */
export interface ControllerDeps {
  stream: LayoutStream;
  entries: EntryInfoProvider;
  /** Creates the GPU renderer; `null` when WebGL2 is unavailable. */
  createRenderer: (canvas: HTMLCanvasElement, view: ViewKind) => ViewRenderer | null;
  /** Selection changes from clicks, marquee and keyboard. */
  select(ids: number[], primary: number | null): void;
  toggleSelect(id: number): void;
  /** Drill: ids below the current root down to the new root. */
  drillTo(chain: number[]): void;
  goUp(): void;
  contextMenu(req: ContextMenuRequest): void;
  /** Screen-reader announcement of the focused entry. */
  announce(text: string): void;
  /** Each new frame (accessible table, status). */
  frame(frame: LayoutFrame): void;
  /** GPU availability changes. */
  glStatus(status: "ok" | "unsupported" | "lost"): void;
  error(err: unknown): void;
}

const VIEW_KIND: Record<VisualView, ViewKind> = {
  treemap: ViewKind.Treemap,
  icicle: ViewKind.Icicle,
  flame: ViewKind.Flame,
  sunburst: ViewKind.Sunburst,
  bubbles: ViewKind.Bubbles,
  mindmap: ViewKind.MindMap,
};

/** Pixels of pointer travel before a press becomes a marquee drag. */
const DRAG_THRESHOLD = 4;
/** Wait after the last wheel tick before asking for a sharper relayout. */
const ZOOM_RELAYOUT_MS = 140;
/** How long the changed-recently pulse runs after a frame arrives. */
const PULSE_MS = 4000;

function cssRgb(el: Element, prop: string, fallback: readonly [number, number, number]): readonly [number, number, number] {
  const v = getComputedStyle(el).getPropertyValue(prop).trim();
  const m = /^#([0-9a-f]{6})$/i.exec(v);
  if (!m?.[1]) return fallback;
  const n = Number.parseInt(m[1], 16);
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
}

/** Drives one canvas. See the module docs. */
export class ViewController {
  private renderer: ViewRenderer | null = null;
  private rendererView: ViewKind | null = null;
  private readonly labels: LabelOverlay | null;
  private frame: LayoutFrame | null = null;
  private picker: Picker | null = null;
  private settings: ViewSettings | null = null;
  private desired: ViewTransform = IDENTITY_TRANSFORM;
  private width = 0;
  private height = 0;
  private dpr = 1;
  private hover = -1;
  private focus = -1;
  private raf = 0;
  private lost = false;
  private disposed = false;
  private zoomTimer: ReturnType<typeof setTimeout> | null = null;
  private pulseUntil = 0;
  private press: { x: number; y: number; button: number; dragging: boolean; pan: ViewTransform | null } | null = null;
  private readonly cleanup: (() => void)[] = [];
  private accent: readonly [number, number, number] = [0.15, 0.39, 0.92];
  private background: readonly [number, number, number] = [0.95, 0.95, 0.95];
  private animating = false;
  private entriesVersion = 0;
  /** Inputs of the last label pass; hover and selection never change labels. */
  private labelKey = "";
  private labelFrame: LayoutFrame | null = null;
  /** CPU time to index and upload the last frame (picker build + GPU upload), in ms. */
  lastFrameLoadMs = 0;
  /** CPU time of the last label pass, in ms (dev harness stats). */
  lastLabelMs = 0;
  /** Measured CPU time of the last draw call, in ms (dev harness stats). */
  lastDrawMs = 0;
  /** Timestamps of recent rendered frames (dev harness stats). */
  readonly frameTimes: number[] = [];
  /** Keep rendering every frame (dev harness benchmark). */
  continuous = false;

  /**
   * @param container - Focusable element wrapping both canvases.
   * @param canvas - WebGL canvas.
   * @param overlay - Canvas2D label overlay, stacked on top.
   * @param marquee - Absolutely positioned element shown while dragging.
   * @param deps - Stream, entries and callbacks.
   */
  constructor(
    private readonly container: HTMLElement,
    private readonly canvas: HTMLCanvasElement,
    private readonly overlay: HTMLCanvasElement,
    private readonly marquee: HTMLElement,
    private readonly deps: ControllerDeps,
  ) {
    const ctx = overlay.getContext("2d");
    this.labels = ctx ? new LabelOverlay(ctx) : null;
    this.cleanup.push(
      deps.stream.subscribe((f) => {
        this.onFrame(f);
      }),
    );
    this.cleanup.push(
      deps.stream.onError((e) => {
        deps.error(e);
      }),
    );
    this.cleanup.push(
      deps.entries.subscribe(() => {
        this.entriesVersion++;
        this.invalidate();
      }),
    );
    this.listen();
    this.observeSize();
  }

  // ---------------------------------------------------------------------------
  // Settings and layout
  // ---------------------------------------------------------------------------

  /**
   * Applies new settings: relayouts only when layout inputs changed; color,
   * theme and selection changes are uniform/byte updates.
   *
   * @param next - Current settings.
   */
  update(next: ViewSettings): void {
    const prev = this.settings;
    this.settings = next;
    const kind = VIEW_KIND[next.view];
    if (this.rendererView !== kind) this.makeRenderer(kind);
    if (prev?.dark !== next.dark) {
      this.renderer?.setTheme(next.dark);
      this.accent = cssRgb(this.container, "--accent-hex", this.accent);
      this.background = cssRgb(this.container, "--canvas-hex", next.dark ? [0.11, 0.11, 0.11] : [0.95, 0.95, 0.95]);
    }
    const rootChanged = !prev || prev.root !== next.root || prev.volumeId !== next.volumeId;
    const layoutChanged =
      !prev ||
      rootChanged ||
      prev.view !== next.view ||
      prev.sizeMode !== next.sizeMode ||
      prev.style !== next.style ||
      prev.filters !== next.filters ||
      // The first known revision describes the index the layout came from.
      (!!prev.revision && prev.revision !== next.revision);
    if (rootChanged || prev.view !== next.view) this.desired = IDENTITY_TRANSFORM;
    if (layoutChanged) {
      const animate = !!prev && prev.volumeId === next.volumeId && prev.view === next.view && rootChanged;
      this.requestLayout(animate);
    }
    if (prev?.selection !== next.selection) {
      this.renderer?.setSelection(new Set(next.selection));
      if (this.frame && next.primary !== null) {
        const i = recordOf(this.frame.nodes, next.primary);
        if (i >= 0) this.focus = i;
      }
    }
    this.invalidate();
  }

  private makeRenderer(kind: ViewKind): void {
    this.renderer?.dispose();
    this.renderer = null;
    this.rendererView = kind;
    try {
      this.renderer = this.deps.createRenderer(this.canvas, kind);
    } catch (err) {
      this.deps.error(err);
    }
    if (!this.renderer) {
      this.deps.glStatus("unsupported");
      return;
    }
    this.deps.glStatus("ok");
    const s = this.settings;
    if (s) {
      this.renderer.setTheme(s.dark);
      this.renderer.setSelection(new Set(s.selection));
    }
    // A frame from another view kind cannot be drawn by this renderer.
    if (this.frame && this.frame.view === kind) this.renderer.setFrame(this.frame, false);
  }

  private requestLayout(animate: boolean): void {
    const s = this.settings;
    if (!s || this.width === 0 || this.height === 0) return;
    const req: LayoutRequest = {
      volumeId: s.volumeId,
      root: s.root,
      view: s.view,
      width: this.width,
      height: this.height,
      dpr: this.dpr,
      transform: s.view === "treemap" ? this.desired : IDENTITY_TRANSFORM,
      sizeMode: s.sizeMode,
      style: s.style,
      filters: s.filters,
      animate: animate && (s.view === "treemap" || s.view === "icicle" || s.view === "flame"),
    };
    this.deps.stream.request(req);
  }

  private onFrame(frame: LayoutFrame): void {
    const kind = this.settings ? VIEW_KIND[this.settings.view] : null;
    if (kind !== null && frame.view !== kind) return;
    const prev = this.frame;
    const t0 = performance.now();
    this.frame = frame;
    this.picker = createPicker(frame);
    const animate = !!prev && frame.transitions !== null && !(this.settings?.reducedMotion ?? false);
    this.renderer?.setFrame(frame, animate);
    this.lastFrameLoadMs = performance.now() - t0;
    this.animating = animate;
    this.hover = -1;
    hoverStore.setState({ target: null });
    const primary = this.settings?.primary ?? null;
    this.focus = primary === null ? -1 : recordOf(frame.nodes, primary);
    if (this.settings?.colorMode === "recent") this.pulseUntil = performance.now() + PULSE_MS;
    this.deps.frame(frame);
    this.invalidate();
  }

  // ---------------------------------------------------------------------------
  // Rendering
  // ---------------------------------------------------------------------------

  /** Schedules one render on the next animation frame. */
  invalidate(): void {
    if (this.raf !== 0 || this.disposed) return;
    this.raf = requestAnimationFrame((now) => {
      this.raf = 0;
      this.render(now);
    });
  }

  private relative(): ViewTransform {
    if (!this.frame) return IDENTITY_TRANSFORM;
    return relativeTransform(this.desired, this.frame.transform);
  }

  private render(now: number): void {
    const s = this.settings;
    if (!s || this.lost) return;
    const view = this.relative();
    const pulsing = s.colorMode === "recent" && !s.reducedMotion && now < this.pulseUntil;
    const state: DrawState = {
      width: this.width,
      height: this.height,
      dpr: this.dpr,
      view,
      colorMode: COLOR_MODE_INDEX[s.colorMode],
      patterns: s.patterns,
      dark: s.dark,
      cushion: s.style === "cushion",
      hover: this.animating ? -1 : this.hover,
      pulse: pulsing ? 0.5 + 0.5 * Math.sin(now / 160) : 1,
      accent: this.accent,
      background: this.background,
      now,
      reducedMotion: s.reducedMotion,
    };
    const t0 = performance.now();
    const more = this.renderer?.draw(state) ?? false;
    this.lastDrawMs = performance.now() - t0;
    this.frameTimes.push(now);
    if (this.frameTimes.length > 600) this.frameTimes.splice(0, this.frameTimes.length - 600);
    const wasAnimating = this.animating;
    this.animating = more;
    const labelKey = `${view.scale}|${view.tx}|${view.ty}|${s.colorMode}|${s.dark}|${s.units}|${this.entriesVersion}|${this.width}x${this.height}`;
    if (more) {
      this.labels?.clear();
      this.labelKey = "";
    } else if (this.frame && this.labels && (wasAnimating || labelKey !== this.labelKey || this.frame !== this.labelFrame)) {
      this.labelKey = labelKey;
      this.labelFrame = this.frame;
      const l0 = performance.now();
      this.labels.draw({
        frame: this.frame,
        view,
        width: this.width,
        height: this.height,
        dpr: this.dpr,
        dark: s.dark,
        colorMode: s.colorMode,
        units: s.units,
        entries: this.deps.entries,
      });
      this.lastLabelMs = performance.now() - l0;
    }
    if (more || pulsing || this.continuous) this.invalidate();
  }

  // ---------------------------------------------------------------------------
  // Sizing
  // ---------------------------------------------------------------------------

  private observeSize(): void {
    const apply = (w: number, h: number) => {
      const width = Math.max(1, Math.round(w));
      const height = Math.max(1, Math.round(h));
      this.dpr = window.devicePixelRatio || 1;
      if (width === this.width && height === this.height) return;
      this.width = width;
      this.height = height;
      this.canvas.width = width;
      this.canvas.height = height;
      this.overlay.width = width;
      this.overlay.height = height;
      this.desired = clampTransform(this.desired, width, height);
      this.requestLayout(false);
      this.invalidate();
    };
    if (typeof ResizeObserver !== "undefined") {
      const ro = new ResizeObserver((entries) => {
        const e = entries[0];
        if (!e) return;
        // NOTE: devicePixelContentBoxSize gives exact device pixels on
        // fractional scales (125%, 150%); multiplying CSS px by DPR can be off
        // by one and blur the canvas.
        const dp = e.devicePixelContentBoxSize[0];
        if (dp) apply(dp.inlineSize, dp.blockSize);
        else apply(e.contentRect.width * window.devicePixelRatio, e.contentRect.height * window.devicePixelRatio);
      });
      try {
        ro.observe(this.canvas, { box: "device-pixel-content-box" });
      } catch {
        ro.observe(this.canvas);
      }
      this.cleanup.push(() => {
        ro.disconnect();
      });
    }
    // Moving the window to a monitor with another scale changes DPR without
    // necessarily resizing the element; re-arm the query on every change.
    let mq: MediaQueryList | null = null;
    const onDpr = () => {
      mq?.removeEventListener("change", onDpr);
      this.dpr = window.devicePixelRatio || 1;
      const r = this.canvas.getBoundingClientRect();
      this.width = 0;
      apply(r.width * this.dpr, r.height * this.dpr);
      arm();
    };
    const arm = () => {
      if (typeof window.matchMedia !== "function") return;
      mq = window.matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
      mq.addEventListener("change", onDpr);
    };
    arm();
    this.cleanup.push(() => mq?.removeEventListener("change", onDpr));
  }

  // ---------------------------------------------------------------------------
  // Pointer and keyboard
  // ---------------------------------------------------------------------------

  /** Converts a client point to frame coordinates (device px, pre-zoom). */
  private toFrame(clientX: number, clientY: number): [number, number] {
    const r = this.canvas.getBoundingClientRect();
    const sx = r.width > 0 ? this.width / r.width : this.dpr;
    const sy = r.height > 0 ? this.height / r.height : this.dpr;
    return invertPoint(this.relative(), (clientX - r.left) * sx, (clientY - r.top) * sy);
  }

  private toDevice(clientX: number, clientY: number): [number, number] {
    const r = this.canvas.getBoundingClientRect();
    const sx = r.width > 0 ? this.width / r.width : this.dpr;
    return [(clientX - r.left) * sx, (clientY - r.top) * sx];
  }

  /**
   * Picks at a client point.
   *
   * @returns Record index or -1.
   */
  pickAt(clientX: number, clientY: number): number {
    if (!this.picker || this.animating) return -1;
    const [x, y] = this.toFrame(clientX, clientY);
    return this.picker.hitTest(x, y);
  }

  private setHover(index: number, clientX: number, clientY: number): void {
    if (index === this.hover) {
      if (index >= 0) hoverStore.setState({ clientX, clientY });
      return;
    }
    this.hover = index;
    const f = this.frame;
    if (index < 0 || !f) {
      hoverStore.setState({ target: null });
    } else {
      const flags = f.nodes.flags(index);
      const agg = (flags & NodeFlag.AGGREGATE) !== 0 ? f.aggregates.forRecord(index) : null;
      hoverStore.setState({
        target: {
          id: f.nodes.id(index),
          index,
          flags,
          aggregate: agg ? { count: agg.count, bytes: agg.bytes } : null,
          rootBytes: f.rootBytes,
        },
        clientX,
        clientY,
      });
    }
    this.invalidate();
  }

  /** Entry ids a record stands for when acted on (aggregates act on nothing). */
  private idsOf(index: number): number[] {
    const f = this.frame;
    if (!f || index < 0 || (f.nodes.flags(index) & NodeFlag.SELECTABLE) === 0) return [];
    return [f.nodes.id(index)];
  }

  /** Drills into the directory at (or containing) `index`. */
  drillAt(index: number): void {
    const f = this.frame;
    if (!f || index < 0) return;
    let target = index;
    const flags = f.nodes.flags(index);
    // Files and aggregates drill into their folder.
    if ((flags & NodeFlag.DIR) === 0 || (flags & NodeFlag.AGGREGATE) !== 0) target = f.nodes.parent(index);
    if (target === NO_INDEX || target === 0) return;
    const chain = [...ancestorIds(f.nodes, target).slice(1), f.nodes.id(target)];
    this.deps.drillTo(chain);
  }

  private listen(): void {
    const c = this.container;
    const on = <K extends keyof HTMLElementEventMap>(type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) => {
      c.addEventListener(type, fn, opts);
      this.cleanup.push(() => {
        c.removeEventListener(type, fn, opts);
      });
    };

    on("pointerdown", (e) => {
      if (e.button !== 0 && e.button !== 1) return;
      c.focus({ preventScroll: true });
      this.press = { x: e.clientX, y: e.clientY, button: e.button, dragging: false, pan: e.button === 1 ? this.desired : null };
      c.setPointerCapture(e.pointerId);
      if (e.button === 1) e.preventDefault();
    });

    on("pointermove", (e) => {
      const p = this.press;
      if (p) {
        const dx = e.clientX - p.x;
        const dy = e.clientY - p.y;
        if (!p.dragging && Math.hypot(dx, dy) > DRAG_THRESHOLD) p.dragging = true;
        if (p.dragging && p.pan) {
          const [ax, ay] = this.toDevice(p.x, p.y);
          const [bx, by] = this.toDevice(e.clientX, e.clientY);
          this.desired = panBy(p.pan, bx - ax, by - ay, this.width, this.height);
          this.scheduleZoomRelayout();
          this.invalidate();
          return;
        }
        if (p.dragging) {
          this.showMarquee(p.x, p.y, e.clientX, e.clientY);
          return;
        }
      }
      this.setHover(this.pickAt(e.clientX, e.clientY), e.clientX, e.clientY);
    });

    on("pointerup", (e) => {
      if (e.button === 3) {
        this.deps.goUp();
        return;
      }
      const p = this.press;
      this.press = null;
      if (!p || e.button !== p.button) return;
      if (p.pan) return;
      if (p.dragging) {
        this.hideMarquee();
        this.selectMarquee(p.x, p.y, e.clientX, e.clientY);
        return;
      }
      const i = this.pickAt(e.clientX, e.clientY);
      const ids = this.idsOf(i);
      const id = ids[0];
      if (e.ctrlKey && id !== undefined) this.deps.toggleSelect(id);
      else this.deps.select(ids, id ?? null);
      this.focus = i;
    });

    on("pointercancel", () => {
      this.press = null;
      this.hideMarquee();
    });

    on("pointerleave", () => {
      if (!this.press) this.setHover(-1, 0, 0);
    });

    on("dblclick", (e) => {
      this.drillAt(this.pickAt(e.clientX, e.clientY));
    });

    on("contextmenu", (e) => {
      e.preventDefault();
      const i = this.pickAt(e.clientX, e.clientY);
      const ids = this.idsOf(i);
      const sel = this.settings?.selection ?? [];
      const id = ids[0];
      let target = ids;
      if (id !== undefined && sel.includes(id)) target = [...sel];
      else this.deps.select(ids, id ?? null);
      this.deps.contextMenu({ ids: target, clientX: e.clientX, clientY: e.clientY });
    });

    on(
      "wheel",
      (e) => {
        if (!this.frame) return;
        e.preventDefault();
        const [x, y] = this.toDevice(e.clientX, e.clientY);
        const delta = e.deltaMode === 1 ? e.deltaY * 33 : e.deltaY;
        this.desired = zoomAbout(this.desired, Math.exp(-delta * 0.0015), x, y, this.width, this.height);
        this.scheduleZoomRelayout();
        this.invalidate();
      },
      { passive: false },
    );

    on("keydown", (e) => {
      this.onKey(e);
    });

    const onLost = (e: Event) => {
      e.preventDefault();
      this.lost = true;
      this.deps.glStatus("lost");
    };
    const onRestored = () => {
      this.lost = false;
      try {
        this.renderer?.restore();
        this.deps.glStatus("ok");
      } catch (err) {
        this.deps.error(err);
      }
      this.invalidate();
    };
    this.canvas.addEventListener("webglcontextlost", onLost);
    this.canvas.addEventListener("webglcontextrestored", onRestored);
    this.cleanup.push(() => {
      this.canvas.removeEventListener("webglcontextlost", onLost);
      this.canvas.removeEventListener("webglcontextrestored", onRestored);
    });
  }

  private scheduleZoomRelayout(): void {
    if (this.settings?.view !== "treemap") return;
    if (this.zoomTimer) clearTimeout(this.zoomTimer);
    this.zoomTimer = setTimeout(() => {
      this.zoomTimer = null;
      if (this.frame && !sameTransform(this.desired, this.frame.transform)) this.requestLayout(false);
    }, ZOOM_RELAYOUT_MS);
  }

  private showMarquee(x0: number, y0: number, x1: number, y1: number): void {
    const r = this.container.getBoundingClientRect();
    const m = this.marquee;
    m.hidden = false;
    m.style.left = `${Math.min(x0, x1) - r.left}px`;
    m.style.top = `${Math.min(y0, y1) - r.top}px`;
    m.style.width = `${Math.abs(x1 - x0)}px`;
    m.style.height = `${Math.abs(y1 - y0)}px`;
  }

  private hideMarquee(): void {
    this.marquee.hidden = true;
  }

  private selectMarquee(cx0: number, cy0: number, cx1: number, cy1: number): void {
    const f = this.frame;
    if (!f || !this.picker) return;
    const [x0, y0] = this.toFrame(cx0, cy0);
    const [x1, y1] = this.toFrame(cx1, cy1);
    const records = marqueeSelect(f, this.picker.subtreeEnd, { x0, y0, x1, y1 });
    const ids = records.map((i) => f.nodes.id(i));
    this.deps.select(ids, ids[ids.length - 1] ?? null);
    this.deps.announce(`${ids.length} ${ids.length === 1 ? "item" : "items"} selected`);
  }

  private onKey(e: KeyboardEvent): void {
    const f = this.frame;
    if (!f || !this.picker) return;
    if (isNavKey(e.key)) {
      e.preventDefault();
      const next = navigate(f.nodes, this.picker.subtreeEnd, this.focus, e.key);
      if (next >= 0 && next !== this.focus) {
        this.focus = next;
        const ids = this.idsOf(next);
        if (ids.length > 0) this.deps.select(ids, ids[0] ?? null);
        this.announceRecord(next);
      }
      return;
    }
    switch (e.key) {
      case "Enter":
        e.preventDefault();
        this.drillAt(this.focus);
        return;
      case "Backspace":
        e.preventDefault();
        this.deps.goUp();
        return;
      case " ": {
        e.preventDefault();
        const id = this.idsOf(this.focus)[0];
        if (id === undefined) return;
        if (e.ctrlKey) this.deps.toggleSelect(id);
        else this.deps.select([id], id);
        return;
      }
      case "Escape":
        if (this.desired.scale !== 1) {
          this.desired = IDENTITY_TRANSFORM;
          this.requestLayout(false);
          this.invalidate();
        } else {
          this.deps.select([], null);
        }
        return;
      case "+":
      case "=":
      case "-":
        e.preventDefault();
        this.desired = zoomAbout(this.desired, e.key === "-" ? 1 / 1.4 : 1.4, this.width / 2, this.height / 2, this.width, this.height);
        this.scheduleZoomRelayout();
        this.invalidate();
        return;
      case "0":
        this.desired = IDENTITY_TRANSFORM;
        this.requestLayout(false);
        this.invalidate();
        return;
      case "ContextMenu":
      case "F10":
        if (e.key === "F10" && !e.shiftKey) return;
        e.preventDefault();
        this.openMenuAtFocus();
        return;
    }
  }

  private openMenuAtFocus(): void {
    const f = this.frame;
    if (!f) return;
    const ids = this.idsOf(this.focus);
    const r = this.canvas.getBoundingClientRect();
    let cx = r.left + r.width / 2;
    let cy = r.top + r.height / 2;
    if (this.focus >= 0 && f.view !== ViewKind.Sunburst) {
      const view = this.relative();
      const k = r.width / Math.max(1, this.width);
      const gx = f.nodes.geom(this.focus, 0);
      const gy = f.nodes.geom(this.focus, 1);
      cx = r.left + (gx * view.scale + view.tx) * k + 8;
      cy = r.top + (gy * view.scale + view.ty) * k + 8;
    }
    const sel = this.settings?.selection ?? [];
    const id = ids[0];
    this.deps.contextMenu({ ids: id !== undefined && sel.includes(id) ? [...sel] : ids, clientX: cx, clientY: cy });
  }

  private announceRecord(i: number): void {
    const f = this.frame;
    if (!f) return;
    const info = this.deps.entries.get(f.nodes.id(i));
    const kind = (f.nodes.flags(i) & NodeFlag.DIR) !== 0 ? "folder" : "file";
    const depth = f.nodes.depth(i);
    this.deps.announce(info ? `${info.name}, ${kind}, level ${depth}` : `${kind}, level ${depth}`);
  }

  /** The canvas box in CSS px (harness benchmark). */
  canvasRect(): DOMRect {
    return this.canvas.getBoundingClientRect();
  }

  /** Current frame (for tests and the harness). */
  get currentFrame(): LayoutFrame | null {
    return this.frame;
  }

  /** Current absolute visual transform. */
  get transform(): ViewTransform {
    return this.desired;
  }

  /** Sets the absolute visual transform (harness benchmark). */
  setTransform(t: ViewTransform): void {
    this.desired = clampTransform(t, this.width, this.height);
    this.invalidate();
  }

  /** Sets the hovered record directly (harness benchmark). */
  setHoverIndex(i: number): void {
    this.hover = i;
    this.invalidate();
  }

  /** Releases listeners, timers and GPU resources. */
  dispose(): void {
    this.disposed = true;
    if (this.raf) cancelAnimationFrame(this.raf);
    if (this.zoomTimer) clearTimeout(this.zoomTimer);
    for (const f of this.cleanup) f();
    this.renderer?.dispose();
    hoverStore.setState({ target: null });
  }
}
