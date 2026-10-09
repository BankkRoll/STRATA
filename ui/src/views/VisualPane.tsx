/**
 * Host for every visual view: mounts the WebGL canvas, the label overlay and
 * the {@link ViewController}, and feeds it the shared state. Also renders the
 * designed loading / error / no-GPU states, the tooltip, the legend and the
 * accessible table alternative of the current map.
 */
import { useEffect, useRef, useState } from "react";
import { errorMessage } from "../lib/backend";
import type { EntryInfoProvider } from "../lib/entries";
import { formatBytes, formatPercent } from "../lib/format";
import { useDark, useEntryInfo, useReducedMotion } from "../lib/hooks";
import { NodeFlag, ViewKind, type LayoutFrame } from "../lib/layout/frame";
import { AGE_BUCKETS, CATEGORIES, SAFETY_TIERS, cssColorFor, encodeColorKey, type ColorMode } from "../lib/palette";
import type { VisualView } from "../lib/types";
import { loadRenderer, rendererLoaded } from "../render/registry";
import { ViewController, type ContextMenuRequest } from "../render/ViewController";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import { Tooltip } from "../components/Tooltip";

const VIEW_KIND: Record<VisualView, ViewKind> = {
  treemap: ViewKind.Treemap,
  icicle: ViewKind.Icicle,
  flame: ViewKind.Flame,
  sunburst: ViewKind.Sunburst,
  bubbles: ViewKind.Bubbles,
  mindmap: ViewKind.MindMap,
};

const VIEW_LABEL: Record<VisualView, string> = {
  treemap: "Treemap",
  icicle: "Icicle chart",
  flame: "Flame chart",
  sunburst: "Sunburst",
  bubbles: "Bubble chart",
  mindmap: "Mind map",
};

/** Props for {@link VisualPane}. */
export interface VisualPaneProps {
  volumeId: string;
  view: VisualView;
  onContextMenu: (req: ContextMenuRequest) => void;
}

type GlStatus = "ok" | "unsupported" | "lost";

function Legend({ mode, dark }: { mode: ColorMode; dark: boolean }) {
  let items: { label: string; color: string }[];
  const zero = { category: 0, safety: 0, age: 0, fileType: 0, app: 0, recent: false };
  if (mode === "category") {
    items = CATEGORIES.map((c) => ({ label: c.label, color: dark ? c.dark : c.light }));
  } else if (mode === "safety") {
    items = SAFETY_TIERS.map((t) => ({ label: t.label, color: dark ? t.dark : t.light }));
  } else if (mode === "age") {
    items = [1, 3, 5, 7, 10, 12, 14, 17, 20].map((b) => ({
      label: AGE_BUCKETS[b]?.label ?? "",
      color: cssColorFor("age", encodeColorKey({ ...zero, age: b }), dark),
    }));
  } else if (mode === "recent") {
    items = [
      { label: "Changed recently", color: cssColorFor("recent", 1 << 30, dark) },
      { label: "Unchanged", color: cssColorFor("recent", 0, dark) },
    ];
  } else {
    return (
      <p className="legend legend--note">
        {mode === "app" ? "Each owning app has its own color; hover for names." : "Each file type has its own color; hover for names."}
      </p>
    );
  }
  return (
    <ul className="legend" aria-label="Color legend">
      {items.map((i) => (
        <li key={i.label}>
          <span className="legend__swatch" style={{ background: i.color }} aria-hidden="true" />
          {i.label}
        </li>
      ))}
    </ul>
  );
}

function TableRow({ provider, id, rootBytes, sizeMode }: { provider: EntryInfoProvider; id: number; rootBytes: number; sizeMode: "allocated" | "logical" }) {
  const info = useEntryInfo(provider, id);
  const units = useSettings((s) => s.units);
  const select = useApp((s) => s.select);
  const size = info ? (sizeMode === "allocated" ? info.allocated : info.logical) : null;
  return (
    <tr>
      <th scope="row">
        <button
          type="button"
          className="linkish"
          onClick={() => {
            select([id], id);
          }}
        >
          {info?.name ?? "Loading…"}
        </button>
      </th>
      <td>{info ? (info.isDir ? "Folder" : "File") : ""}</td>
      <td>{size === null ? "" : formatBytes(size, { units })}</td>
      <td>{size === null || rootBytes === 0 ? "" : formatPercent(size / rootBytes)}</td>
    </tr>
  );
}

/** Accessible alternative to the canvas: the root's direct children as a table. */
function MapTable({ frame, provider, view, sizeMode, visible }: { frame: LayoutFrame; provider: EntryInfoProvider; view: VisualView; sizeMode: "allocated" | "logical"; visible: boolean }) {
  const ids: number[] = [];
  let hidden = 0;
  for (let i = 1; i < frame.nodes.count; i++) {
    if (frame.nodes.parent(i) !== 0) continue;
    if ((frame.nodes.flags(i) & NodeFlag.SELECTABLE) !== 0) ids.push(frame.nodes.id(i));
    else hidden++;
  }
  return (
    <section className={visible ? "map-table" : "map-table visually-hidden"} aria-label={`${VIEW_LABEL[view]} as a table`}>
      <table>
        <caption>
          Largest items in this {VIEW_LABEL[view].toLowerCase()}
          {hidden > 0 ? ", plus small items grouped together" : ""}
        </caption>
        <thead>
          <tr>
            <th scope="col">Name</th>
            <th scope="col">Kind</th>
            <th scope="col">Size</th>
            <th scope="col">Share</th>
          </tr>
        </thead>
        <tbody>
          {ids.map((id) => (
            <TableRow key={id} provider={provider} id={id} rootBytes={frame.rootBytes} sizeMode={sizeMode} />
          ))}
        </tbody>
      </table>
    </section>
  );
}

/** The visual view host. */
export function VisualPane({ volumeId, view, onContextMenu }: VisualPaneProps) {
  const services = useServices();
  const containerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLCanvasElement>(null);
  const marqueeRef = useRef<HTMLDivElement>(null);
  const controllerRef = useRef<ViewController | null>(null);
  const [gl, setGl] = useState<GlStatus>("ok");
  const [error, setError] = useState<string | null>(null);
  const [frame, setFrame] = useState<LayoutFrame | null>(null);
  const [announcement, setAnnouncement] = useState("");
  const [showTable, setShowTable] = useState(false);
  const dark = useDark();
  const reducedMotion = useReducedMotion();
  const patterns = useSettings((s) => s.patterns);
  const units = useSettings((s) => s.units);
  const sizeMode = useApp((s) => s.sizeMode);
  const colorMode = useApp((s) => s.colorMode);
  const provider = services.entryInfo(volumeId);
  const menuRef = useRef(onContextMenu);
  useEffect(() => {
    menuRef.current = onContextMenu;
  }, [onContextMenu]);
  const [, setLoadedCount] = useState(0);
  const rendererReady = rendererLoaded(VIEW_KIND[view]);

  useEffect(() => {
    let cancelled = false;
    const kind = VIEW_KIND[view];
    if (rendererLoaded(kind)) return;
    loadRenderer(kind)
      .then(() => {
        if (!cancelled) setLoadedCount((n) => n + 1);
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(errorMessage(err));
      });
    return () => {
      cancelled = true;
    };
  }, [view]);

  useEffect(() => {
    const container = containerRef.current;
    const canvas = canvasRef.current;
    const overlay = overlayRef.current;
    const marquee = marqueeRef.current;
    if (!container || !canvas || !overlay || !marquee) return;
    const stream = services.createLayoutStream();
    const app = useApp.getState;
    const controller = new ViewController(container, canvas, overlay, marquee, {
      stream,
      entries: provider,
      createRenderer: (c, k) => services.createRenderer(c, k),
      select: (ids, primary) => {
        app().select(ids, primary);
      },
      toggleSelect: (id) => {
        app().toggleSelect(id);
      },
      drillTo: (chain) => {
        app().drillTo(chain);
      },
      goUp: () => {
        app().goUp();
      },
      contextMenu: (req) => {
        menuRef.current(req);
      },
      announce: setAnnouncement,
      frame: (f) => {
        setError(null);
        setFrame(f);
      },
      glStatus: setGl,
      error: (err) => {
        console.error("visual view", err);
        setError(errorMessage(err));
      },
    });
    controllerRef.current = controller;
    if (import.meta.env.DEV) (window as unknown as { __strataView?: ViewController }).__strataView = controller;
    return () => {
      controller.dispose();
      stream.close();
      controllerRef.current = null;
    };
  }, [services, provider]);

  useEffect(() => {
    const c = controllerRef.current;
    if (!c || !rendererReady) return;
    const push = () => {
      const s = useApp.getState();
      const root = s.path[s.path.length - 1];
      if (root === undefined) return;
      c.update({
        volumeId,
        root,
        view,
        sizeMode: s.sizeMode,
        style: s.treemapStyle,
        filters: s.filters,
        colorMode: s.colorMode,
        patterns,
        dark,
        units,
        reducedMotion,
        selection: s.selection,
        primary: s.primary,
      });
    };
    push();
    return useApp.subscribe(push);
  }, [volumeId, view, patterns, dark, units, reducedMotion, rendererReady, services, provider]);

  const loading = frame === null || frame.view !== VIEW_KIND[view];

  return (
    <section className="visual" aria-label={VIEW_LABEL[view]}>
      <div
        ref={containerRef}
        className="visual__surface"
        tabIndex={0}
        role="application"
        aria-roledescription={VIEW_LABEL[view].toLowerCase()}
        aria-label={`${VIEW_LABEL[view]}. Arrow keys move between items, Enter opens a folder, Backspace goes up, Shift+F10 opens actions.`}
        aria-describedby="visual-live"
      >
        <canvas ref={canvasRef} className="visual__canvas" aria-hidden="true" />
        <canvas ref={overlayRef} className="visual__overlay" aria-hidden="true" />
        <div ref={marqueeRef} className="visual__marquee" hidden />
        {gl === "unsupported" && (
          <div className="state state--error" role="alert">
            <h2>Graphics acceleration is unavailable</h2>
            <p>The map needs WebGL 2. The list pane still shows everything; update your graphics driver to bring the map back.</p>
          </div>
        )}
        {gl === "lost" && (
          <div className="state" role="status">
            <h2>Restoring graphics…</h2>
            <p>The graphics driver reset. The map comes back automatically.</p>
          </div>
        )}
        {gl === "ok" && error && (
          <div className="state state--error" role="alert">
            <h2>Can’t draw this view</h2>
            <p>{error}</p>
          </div>
        )}
        {gl === "ok" && !error && loading && (
          <div className="state state--quiet" role="status">
            <span className="spinner" aria-hidden="true" />
            <p>Laying out…</p>
          </div>
        )}
      </div>
      <div id="visual-live" className="visually-hidden" aria-live="polite">
        {announcement}
      </div>
      <Tooltip provider={provider} sizeMode={sizeMode} />
      <footer className="visual__footer">
        <Legend mode={colorMode} dark={dark} />
        <button
          type="button"
          className="linkish"
          aria-expanded={showTable}
          onClick={() => {
            setShowTable(!showTable);
          }}
        >
          {showTable ? "Hide table" : "Show as table"}
        </button>
      </footer>
      {frame && !loading && <MapTable frame={frame} provider={provider} view={view} sizeMode={sizeMode} visible={showTable} />}
    </section>
  );
}
