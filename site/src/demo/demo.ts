/**
 * The interactive product demo: Strata's own {@link ViewController} and
 * WebGL2 renderers, drawing the layout frames `strata-layout` exports for the
 * UI tests, inside a window built from the app's styles.
 *
 * Responsibilities:
 * - Wire the controller to the fixture stream and entry lookups.
 * - Drive the chrome: breadcrumbs, view and color switches, legend, tooltip,
 *   detail strip and hint line.
 * - Click-to-drill into the one folder the fixtures include a drill frame
 *   for (outlined), with the real animated transition both ways.
 * - Fall back to an SVG drawing of the same buffer without WebGL2.
 *
 * Page-friendly deviations from the app: the wheel scrolls the page unless
 * Ctrl is held, and touch drags scroll instead of starting a marquee.
 */
import { formatBytes, formatCount, formatPercent } from "@ui/lib/format";
import { NodeFlag, ViewKind, type LayoutFrame } from "@ui/lib/layout/frame";
import { recordOf } from "@ui/lib/layout/navigate";
import { ancestorIds } from "@ui/lib/layout/pick";
import { relativeTransform } from "@ui/lib/layout/transform";
import { AGE_BUCKETS, CATEGORIES, SAFETY_TIERS, categoryInfo, cssColorFor, encodeColorKey, decodeColorKey, type ColorMode } from "@ui/lib/palette";
import { NO_FILTERS, type VisualView } from "@ui/lib/types";
import { createRenderer, loadRenderer } from "@ui/render/registry";
import { ViewController } from "@ui/render/ViewController";
import { hoverStore } from "@ui/store/hover";
import { drawFallback } from "./fallback";
import { DemoLayoutStream, Fixtures, ROOT_NAME, type DemoEntries } from "./fixtures";

const VIEW_KIND: Partial<Record<VisualView, ViewKind>> = {
  treemap: ViewKind.Treemap,
  sunburst: ViewKind.Sunburst,
  icicle: ViewKind.Icicle,
  bubbles: ViewKind.Bubbles,
};

const VIEW_LABEL: Partial<Record<VisualView, string>> = {
  treemap: "Treemap",
  sunburst: "Sunburst",
  icicle: "Icicle chart",
  bubbles: "Bubble chart",
};

const SAFETY_LABEL = { safe: "Safe to remove", probably: "Probably safe", careful: "Careful", never: "Never delete" } as const;
const ZERO_KEY = { category: 0, safety: 0, age: 0, fileType: 0, app: 0, recent: false };

/** Mutable demo state; every change goes through {@link Demo.push}. */
interface State {
  view: VisualView;
  path: number[];
  colorMode: ColorMode;
  selection: number[];
  primary: number | null;
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function query<T extends Element>(root: ParentNode, sel: string): T {
  const e = root.querySelector<T>(sel);
  if (!e) throw new Error(`demo markup is missing ${sel}`);
  return e;
}

/** One mounted demo. */
class Demo {
  private readonly controller: ViewController;
  private readonly entries: DemoEntries;
  private readonly state: State;
  private frame: LayoutFrame | null = null;
  private fallback = false;
  private noticeTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly dark = window.matchMedia("(prefers-color-scheme: dark)");
  private readonly motion = window.matchMedia("(prefers-reduced-motion: reduce)");

  private readonly surface: HTMLElement;
  private readonly canvas: HTMLCanvasElement;
  private readonly cue: HTMLElement;
  private readonly svg: SVGSVGElement;
  private readonly crumbs: HTMLOListElement;
  private readonly legend: HTMLUListElement;
  private readonly hint: HTMLElement;
  private readonly detail: HTMLElement;
  private readonly live: HTMLElement;
  private readonly status: HTMLElement;
  private readonly tip: HTMLElement;

  constructor(
    private readonly root: HTMLElement,
    private readonly fixtures: Fixtures,
    forceFallback: boolean,
  ) {
    this.entries = fixtures.entries;
    this.surface = query(root, "[data-surface]");
    this.canvas = query(root, "[data-canvas]");
    this.cue = el("div", "drill-cue");
    this.cue.append(el("span", "drill-cue__tag", "Click to open"));
    this.cue.setAttribute("aria-hidden", "true");
    this.cue.hidden = true;
    this.surface.append(this.cue);
    this.svg = query(root, "[data-fallback]");
    this.crumbs = query(root, "[data-crumbs]");
    this.legend = query(root, "[data-legend]");
    this.hint = query(root, "[data-hint]");
    this.detail = query(root, "[data-detail]");
    this.live = query(root, "[data-live]");
    this.status = query(root, "[data-state]");
    this.tip = query(document, "[data-tip-host]");

    const reduced = this.motion.matches;
    // NOTE: start inside the drill folder and zoom out once the window is on
    // screen, so the first thing visitors see is the real drill-up animation.
    this.state = {
      view: "treemap",
      path: reduced ? [0] : [0, fixtures.drillRoot],
      colorMode: "category",
      selection: [],
      primary: null,
    };

    const stream = new DemoLayoutStream(fixtures);
    this.controller = new ViewController(this.surface, this.canvas, query(root, "[data-overlay]"), query(root, "[data-marquee]"), {
      stream,
      entries: this.entries,
      createRenderer: (canvas, kind) => (forceFallback ? null : createRenderer(canvas, kind)),
      select: (ids, primary) => {
        this.state.selection = ids;
        this.state.primary = primary;
        this.push();
      },
      toggleSelect: (id) => {
        const s = this.state.selection;
        this.state.selection = s.includes(id) ? s.filter((x) => x !== id) : [...s, id];
        this.state.primary = id;
        this.push();
      },
      drillTo: (chain) => {
        this.drill(chain);
      },
      goUp: () => {
        this.goUp();
      },
      contextMenu: () => {
        this.notice("Actions like Open, Reveal and Clean up live in the app.");
      },
      announce: (text) => {
        this.live.textContent = text;
      },
      frame: (f) => {
        this.onFrame(f);
      },
      glStatus: (s) => {
        this.onGlStatus(s);
      },
      error: (err) => {
        console.error("demo", err);
        this.showState("Can’t draw this view", err instanceof Error ? err.message : String(err), true);
      },
    });

    this.wireControls();
    this.wirePointer();
    this.wireTooltip();
    this.dark.addEventListener("change", () => {
      this.push();
    });
    this.motion.addEventListener("change", () => {
      this.push();
    });
    this.push();
  }

  // ---------------------------------------------------------------------------
  // State
  // ---------------------------------------------------------------------------

  private get rootId(): number {
    return this.state.path[this.state.path.length - 1] ?? 0;
  }

  private get canDrill(): boolean {
    return this.state.view === "treemap" && this.rootId === 0;
  }

  /** Sends the current state to the controller and refreshes the chrome. */
  private push(): void {
    const s = this.state;
    const selection = this.canDrill ? [this.fixtures.drillRoot, ...s.selection] : s.selection;
    this.controller.update({
      volumeId: "demo",
      root: this.rootId,
      view: s.view,
      sizeMode: "allocated",
      style: "flat",
      filters: NO_FILTERS,
      colorMode: s.colorMode,
      patterns: false,
      dark: this.dark.matches,
      units: "binary",
      reducedMotion: this.motion.matches,
      selection,
      primary: s.primary,
    });
    if (!this.canDrill) this.cue.hidden = true;
    this.renderCrumbs();
    this.renderLegend();
    this.renderDetail();
    this.renderHint();
    this.syncRadios("[data-views]", s.view);
    this.syncRadios("[data-colors]", s.colorMode);
    if (this.fallback && this.frame) drawFallback(this.svg, this.frame, s.colorMode, this.dark.matches);
  }

  /** Zooms out from the drill folder (the intro), once. */
  playIntro(): void {
    if (this.state.path.length > 1 && this.state.view === "treemap") this.goUp();
  }

  private drill(chain: number[]): void {
    if (this.state.view !== "treemap") {
      this.notice("Drill-down is wired up in the treemap view of this demo.");
      return;
    }
    if (chain[0] !== this.fixtures.drillRoot || this.rootId !== 0) {
      this.notice("This demo ships one recorded drill-down: the outlined folder.");
      return;
    }
    this.cue.hidden = true;
    this.state.path = [0, this.fixtures.drillRoot];
    this.push();
    this.live.textContent = `Opened ${this.entries.name(this.fixtures.drillRoot)}`;
  }

  private goUp(): void {
    if (this.state.path.length <= 1) return;
    this.state.path = [0];
    this.state.selection = [];
    this.state.primary = null;
    this.push();
    this.live.textContent = `Back to ${ROOT_NAME}`;
  }

  private onFrame(f: LayoutFrame): void {
    this.frame = f;
    this.status.hidden = true;
    if (this.fallback) drawFallback(this.svg, f, this.state.colorMode, this.dark.matches);
    this.renderLegend();
    this.renderDetail();
    this.placeCue(f.transitions !== null);
  }

  /**
   * Outlines the drillable folder with a DOM overlay. The renderer's own
   * selection ring marks it too, but at page scale a 2-pixel ring is easy to
   * miss.
   *
   * @param afterTransition - Fade in once the drill-up animation has played.
   */
  private placeCue(afterTransition = false): void {
    const cue = this.cue;
    const f = this.frame;
    const i = f && this.canDrill && f.view === ViewKind.Treemap && f.root === 0 ? recordOf(f.nodes, this.fixtures.drillRoot) : -1;
    if (!f || i < 0 || this.controller.transform.scale !== 1) {
      cue.hidden = true;
      return;
    }
    const r = relativeTransform(this.controller.transform, f.transform);
    const k = this.canvas.clientWidth / Math.max(1, this.canvas.width);
    const x = (f.nodes.geom(i, 0) * r.scale + r.tx) * k;
    const y = (f.nodes.geom(i, 1) * r.scale + r.ty) * k;
    cue.style.left = `${x}px`;
    cue.style.top = `${y}px`;
    cue.style.width = `${f.nodes.geom(i, 2) * r.scale * k}px`;
    cue.style.height = `${f.nodes.geom(i, 3) * r.scale * k}px`;
    cue.classList.toggle("is-delayed", afterTransition && !this.motion.matches);
    if (cue.hidden) {
      cue.hidden = false;
      cue.classList.remove("is-on");
      void cue.offsetWidth;
      cue.classList.add("is-on");
    }
  }

  private onGlStatus(s: "ok" | "unsupported" | "lost"): void {
    if (s === "unsupported") {
      this.fallback = true;
      this.root.classList.add("is-fallback");
      this.svg.removeAttribute("hidden");
      if (this.frame) drawFallback(this.svg, this.frame, this.state.colorMode, this.dark.matches);
    } else if (s === "lost") {
      this.showState("Restoring graphics…", "The graphics driver reset. The map comes back automatically.", false);
    } else {
      this.status.hidden = this.frame !== null;
    }
  }

  private showState(title: string, text: string, error: boolean): void {
    const st = this.status;
    st.hidden = false;
    st.className = error ? "state state--error" : "state";
    st.replaceChildren(el("h2", undefined, title), el("p", undefined, text));
  }

  private notice(text: string): void {
    this.hint.textContent = text;
    this.hint.classList.add("is-notice");
    this.live.textContent = text;
    if (this.noticeTimer) clearTimeout(this.noticeTimer);
    this.noticeTimer = setTimeout(() => {
      this.hint.classList.remove("is-notice");
      this.renderHint();
    }, 3200);
  }

  // ---------------------------------------------------------------------------
  // Chrome
  // ---------------------------------------------------------------------------

  private renderCrumbs(): void {
    const items = this.state.path.map((id, i) => {
      const li = el("li", "crumbs__item");
      if (i > 0) li.append(chevron());
      const label = i === 0 ? ROOT_NAME : this.entries.name(id);
      if (i === this.state.path.length - 1) {
        const cur = el("span", "crumbs__current", label);
        cur.setAttribute("aria-current", "page");
        li.append(cur);
      } else {
        const b = el("button", "crumbs__link", label);
        b.type = "button";
        b.addEventListener("click", () => {
          this.goUp();
        });
        li.append(b);
      }
      return li;
    });
    this.crumbs.replaceChildren(...items);
  }

  private renderLegend(): void {
    const dark = this.dark.matches;
    const mode = this.state.colorMode;
    let items: { label: string; color: string }[] = [];
    if (mode === "category") {
      const bytes = new Map<number, number>();
      const f = this.frame;
      if (f) {
        for (let i = 0; i < f.nodes.count; i++) {
          if ((f.nodes.flags(i) & NodeFlag.DIR) !== 0) continue;
          const c = decodeColorKey(f.nodes.colorKey(i)).category;
          bytes.set(c, (bytes.get(c) ?? 0) + f.nodes.geom(i, 2) * f.nodes.geom(i, 3));
        }
      }
      items = CATEGORIES.filter((c) => bytes.size === 0 || bytes.has(c.id))
        .sort((a, b) => (bytes.get(b.id) ?? 0) - (bytes.get(a.id) ?? 0))
        .map((c) => ({ label: c.label, color: dark ? c.dark : c.light }));
    } else if (mode === "safety") {
      items = SAFETY_TIERS.map((t) => ({ label: t.label, color: dark ? t.dark : t.light }));
    } else if (mode === "age") {
      items = [1, 3, 5, 7, 10, 12, 14, 17, 20].map((b) => ({
        label: AGE_BUCKETS[b]?.label ?? "",
        color: cssColorFor("age", encodeColorKey({ ...ZERO_KEY, age: b }), dark),
      }));
    }
    this.legend.replaceChildren(
      ...items.map((i) => {
        const li = el("li");
        const sw = el("span", "legend__swatch");
        sw.style.background = i.color;
        sw.setAttribute("aria-hidden", "true");
        li.append(sw, i.label);
        return li;
      }),
    );
  }

  private renderHint(): void {
    if (this.hint.classList.contains("is-notice")) return;
    let text: string;
    if (this.state.view !== "treemap") text = "Hover for details. Drill-down is wired up in the treemap.";
    else if (this.canDrill) text = "Click the outlined folder to drill in.";
    else text = "Backspace or the breadcrumb goes back up.";
    this.hint.textContent = text;
  }

  private renderDetail(): void {
    const id = this.state.primary ?? this.rootId;
    const info = this.entries.get(id);
    const f = this.frame;
    const d = this.detail;
    if (!info) return;
    const dark = this.dark.matches;
    const key = this.entries.colorKey(id);
    const k = decodeColorKey(key);
    const sw = el("span", "detail-strip__swatch");
    sw.style.background = cssColorFor(this.state.colorMode, key, dark);
    sw.setAttribute("aria-hidden", "true");

    const main = el("div", "detail-strip__main");
    const many = this.state.selection.length > 1;
    main.append(el("strong", "detail-strip__name", many ? `${formatCount(this.state.selection.length)} items selected` : info.name));
    const path = this.entries.pathTo(id).slice(0, -1).map((p) => this.entries.name(p));
    main.append(el("span", "detail-strip__path", path.length > 0 ? path.join(" \\ ") : info.isDir ? "Volume root" : ""));

    const facts = el("dl", "detail-strip__facts");
    const fact = (dt: string, dd: string) => {
      const div = el("div");
      div.append(el("dt", undefined, dt), el("dd", undefined, dd));
      facts.append(div);
    };
    const rootBytes = f?.rootBytes ?? 0;
    fact("Size", formatBytes(info.allocated));
    if (id !== this.rootId && rootBytes > 0) fact("Share", formatPercent(info.allocated / rootBytes));
    if (info.isDir) fact("Items", formatCount(info.items));
    fact("Category", categoryInfo(info.category).label);
    fact("Modified", AGE_BUCKETS[k.age]?.label ?? "Unknown");

    const tier = el("span", info.safety ? `tier tier--${info.safety}` : "tier detail-strip__muted", info.safety ? SAFETY_LABEL[info.safety] : "Not classified");
    const perf = el("span", "detail-strip__perf");
    if (f) {
      const ms = this.controller.lastFrameLoadMs;
      perf.textContent = `${formatCount(f.nodes.count)} ${this.fallback ? "SVG shapes" : "GPU instances"} · ingest ${ms < 1 ? ms.toFixed(2) : ms.toFixed(1)} ms`;
    }
    d.replaceChildren(sw, main, facts, tier, perf);
  }

  private syncRadios(group: string, value: string): void {
    for (const b of this.root.querySelectorAll<HTMLButtonElement>(`${group} [role="radio"]`)) {
      const on = b.dataset.value === value;
      b.setAttribute("aria-checked", String(on));
      b.tabIndex = on ? 0 : -1;
    }
  }

  // ---------------------------------------------------------------------------
  // Input
  // ---------------------------------------------------------------------------

  private wireControls(): void {
    const radios = (group: string, apply: (v: string) => void) => {
      const g = query<HTMLElement>(this.root, group);
      const buttons = [...g.querySelectorAll<HTMLButtonElement>('[role="radio"]')];
      for (const b of buttons) {
        b.addEventListener("click", () => {
          apply(b.dataset.value ?? "");
        });
        b.addEventListener("keydown", (e) => {
          if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
          e.preventDefault();
          const i = buttons.indexOf(b) + (e.key === "ArrowRight" ? 1 : -1);
          const next = buttons[(i + buttons.length) % buttons.length];
          if (!next) return;
          apply(next.dataset.value ?? "");
          next.focus();
        });
      }
    };
    radios("[data-views]", (v) => {
      void this.setView(v as VisualView);
    });
    radios("[data-colors]", (v) => {
      this.state.colorMode = v as ColorMode;
      this.push();
    });
  }

  private async setView(view: VisualView): Promise<void> {
    const kind = VIEW_KIND[view];
    if (kind === undefined || view === this.state.view) return;
    await loadRenderer(kind);
    this.state.view = view;
    this.state.path = [0];
    this.state.selection = [];
    this.state.primary = null;
    const label = VIEW_LABEL[view] ?? view;
    this.surface.setAttribute("aria-roledescription", label.toLowerCase());
    this.surface.setAttribute("aria-label", `${label} of a synthetic volume. Arrow keys move between items, Enter opens a folder, Backspace goes up.`);
    this.surface.closest("section")?.setAttribute("aria-label", label);
    this.push();
  }

  private wirePointer(): void {
    const parent = this.surface.parentElement ?? this.surface;
    // The page scrolls with the wheel; Ctrl+wheel zooms like the app does.
    parent.addEventListener(
      "wheel",
      (e) => {
        if (!e.ctrlKey || this.fallback) e.stopPropagation();
      },
      { capture: true },
    );
    // Visual zoom is GPU-side; the SVG fallback cannot follow it.
    parent.addEventListener(
      "keydown",
      (e) => {
        if (this.fallback && ["+", "=", "-", "0"].includes(e.key)) e.stopPropagation();
      },
      { capture: true },
    );
    // Touch drags scroll the page instead of drawing a marquee.
    parent.addEventListener(
      "pointerdown",
      (e) => {
        if (e.pointerType === "touch") e.stopPropagation();
      },
      { capture: true },
    );
    const recue = () => {
      requestAnimationFrame(() => {
        this.placeCue();
      });
    };
    this.surface.addEventListener("wheel", recue, { passive: true });
    this.surface.addEventListener("keyup", recue);
    this.surface.addEventListener("click", (e) => {
      const f = this.frame;
      if (!f) return;
      const i = this.controller.pickAt(e.clientX, e.clientY);
      if (i < 0) return;
      const drillRoot = this.fixtures.drillRoot;
      const inside = (f.nodes.id(i) === drillRoot && (f.nodes.flags(i) & NodeFlag.AGGREGATE) === 0) || ancestorIds(f.nodes, i).includes(drillRoot);
      if (this.canDrill && inside) this.drill([drillRoot]);
      else if ((e as PointerEvent).pointerType === "touch") {
        const id = f.nodes.id(i);
        if ((f.nodes.flags(i) & NodeFlag.SELECTABLE) !== 0) {
          this.state.selection = [id];
          this.state.primary = id;
          this.push();
        }
      }
    });
  }

  private wireTooltip(): void {
    const tip = el("div", "tip");
    tip.setAttribute("role", "tooltip");
    tip.hidden = true;
    this.tip.append(tip);
    let shown = -2;
    const place = () => {
      const { clientX, clientY } = hoverStore.getState();
      const w = tip.offsetWidth;
      const h = tip.offsetHeight;
      tip.style.left = `${clientX + 16 + w > window.innerWidth ? clientX - w - 12 : clientX + 16}px`;
      tip.style.top = `${clientY + 18 + h > window.innerHeight ? clientY - h - 12 : clientY + 18}px`;
    };
    hoverStore.subscribe((s) => {
      const t = s.target;
      if (!t) {
        tip.hidden = true;
        shown = -2;
        return;
      }
      if (t.index !== shown) {
        shown = t.index;
        tip.replaceChildren(...this.tooltipBody(t.id, t.flags, t.aggregate, t.rootBytes));
        tip.hidden = false;
      }
      place();
    });
    window.addEventListener(
      "scroll",
      () => {
        if (!tip.hidden) {
          tip.hidden = true;
          shown = -2;
        }
      },
      { passive: true },
    );
  }

  private tooltipBody(id: number, flags: number, aggregate: { count: number; bytes: number } | null, rootBytes: number): Node[] {
    const grid = el("dl", "tip__grid");
    const row = (dt: string, dd: string) => {
      grid.append(el("dt", undefined, dt), el("dd", undefined, dd));
    };
    if ((flags & NodeFlag.AGGREGATE) !== 0) {
      const name = el("div", "tip__name", aggregate ? `${formatCount(aggregate.count)} small items` : "Small items");
      if (aggregate) {
        row("Size", formatBytes(aggregate.bytes));
        row("Share", formatPercent(rootBytes > 0 ? aggregate.bytes / rootBytes : 0));
      }
      return [name, grid, el("div", "tip__hint", "Too small to draw individually. Zoom in or drill down.")];
    }
    const info = this.entries.get(id);
    if (!info) return [el("div", "tip__name", "Unknown entry")];
    const k = decodeColorKey(this.entries.colorKey(id));
    row("Size", formatBytes(info.allocated));
    row("Share", `${formatPercent(rootBytes > 0 ? info.allocated / rootBytes : 0)} of this view`);
    if (info.isDir) row("Items", formatCount(info.items));
    row("Category", categoryInfo(info.category).label);
    row("App", info.app ?? "Not attributed");
    row("Modified", AGE_BUCKETS[k.age]?.label ?? "Unknown");
    row("Safety", info.safety ? SAFETY_LABEL[info.safety] : "Not classified");
    const out: Node[] = [el("div", "tip__name", info.name), grid];
    if (this.canDrill && (id === this.fixtures.drillRoot || this.entries.pathTo(id).includes(this.fixtures.drillRoot))) {
      out.push(el("div", "tip__hint", "Click to drill in."));
    }
    return out;
  }
}

function chevron(): SVGSVGElement {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 16 16");
  s.setAttribute("width", "12");
  s.setAttribute("height", "12");
  s.setAttribute("aria-hidden", "true");
  s.setAttribute("class", "crumbs__sep");
  s.innerHTML = '<path d="M6 3.5 10.5 8 6 12.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>';
  return s;
}

/** Handle returned by {@link mountDemo}. */
export interface DemoHandle {
  /** Plays the zoom-out intro (no-op with reduced motion or after use). */
  playIntro(): void;
}

/**
 * Loads the fixtures and mounts the demo into the window markup in `root`.
 *
 * @param root - The `.strata-app` window element from `index.html`.
 * @returns A handle for the scroll-driven intro.
 */
export async function mountDemo(root: HTMLElement): Promise<DemoHandle> {
  const fixtures = await Fixtures.load();
  const force = new URLSearchParams(location.search).has("nogl");
  const demo = new Demo(root, fixtures, force);
  root.classList.add("is-ready");
  return demo;
}
