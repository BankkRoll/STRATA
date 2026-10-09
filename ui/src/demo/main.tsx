/**
 * Entry of the website demo (`index.html` next to this file, built by
 * `vite.demo.config.ts`): the real app shell and stylesheet over the sample
 * tree, with {@link demoServices} in place of the engine.
 *
 * Responsibilities:
 * - Load `tree.bin`, open the sample volume on the treemap and render
 *   {@link AppShell} with Strata's own title bar (window buttons do nothing
 *   outside the desktop shell).
 * - When embedded in the website: let the page scroll under the pointer
 *   (Ctrl + wheel zooms the map, as pinch does), and hand focus back to the
 *   page on Escape when the app has nothing of its own to close.
 */
import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { AppShell } from "../components/AppShell";
import treeUrl from "../lib/layout/__fixtures__/tree.bin?url";
import { applyBackdrop, applyTheme } from "../lib/theme";
import { ServicesContext, type Services } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import "../styles.css";
import { demoServices } from "./services";

/** Message the demo posts to the embedding page when the visitor presses Escape to leave. */
export const LEAVE_MESSAGE = "strata-demo:leave";

/**
 * `App` without the startup `app_info` call, which needs the engine; the
 * demo paints its own (solid) background like the app does without Mica.
 */
function DemoApp({ services }: { services: Services }) {
  const theme = useSettings((s) => s.theme);
  useEffect(() => {
    applyTheme(document.documentElement, theme);
  }, [theme]);
  useEffect(() => {
    applyBackdrop(document.documentElement, "solid");
  }, []);
  return (
    <ServicesContext value={services}>
      <AppShell />
    </ServicesContext>
  );
}

/** Escape closes these first; only with none open does it leave the demo. */
const CLOSABLE = '[role="dialog"], [role="menu"], [role="listbox"], .inspector--overlay, .scrim';

function isTextField(el: Element | null): boolean {
  return el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement || (el instanceof HTMLElement && el.isContentEditable);
}

/** Message the embedding page posts with the scale it draws the frame at. */
export const SCALE_MESSAGE = "strata-demo:scale";

/**
 * Makes `devicePixelRatio` include the scale the page draws the frame at.
 *
 * The page shrinks the 1280×800 frame with a CSS transform. Chromium then
 * reports the canvas's device-pixel size after that transform but keeps the
 * monitor's ratio, so the map would lay out a small canvas at full-size
 * spacing and oversized labels. With the scaled ratio both agree, and the
 * map renders exactly as the app does at that display scale.
 *
 * @param initial - Scale from the frame URL.
 */
function followPageScale(initial: number): void {
  const own = Object.getOwnPropertyDescriptor(window, "devicePixelRatio") ?? Object.getOwnPropertyDescriptor(Window.prototype, "devicePixelRatio");
  const native = () => (own?.get ? (own.get.call(window) as number) : 1);
  let scale = initial;
  Object.defineProperty(window, "devicePixelRatio", { configurable: true, get: () => native() * scale });
  window.addEventListener("message", (e: MessageEvent<unknown>) => {
    if (e.origin !== location.origin || e.source !== window.parent) return;
    const d = e.data as { type?: unknown; scale?: unknown } | null;
    if (d?.type === SCALE_MESSAGE && typeof d.scale === "number" && d.scale > 0 && d.scale <= 1) scale = d.scale;
  });
}

/** Page-friendly behavior when the demo runs inside the website's frame. */
function embed(): void {
  if (window.parent === window) return;
  const scale = Number(new URLSearchParams(location.search).get("scale"));
  followPageScale(scale > 0 && scale <= 1 ? scale : 1);
  // The map zooms on every wheel event in the app. Inside a web page that
  // would trap scrolling, so a plain wheel scrolls the page and Ctrl + wheel
  // (and pinch, which arrives as one) still zooms.
  window.addEventListener(
    "wheel",
    (e) => {
      if (!e.ctrlKey && e.target instanceof Element && e.target.closest(".visual__surface")) e.stopPropagation();
    },
    { capture: true, passive: true },
  );
  // NOTE: capture phase, so the check sees the state before the app's own
  // handlers close a menu or cancel an edit with the same key press.
  window.addEventListener(
    "keydown",
    (e) => {
      if (e.key !== "Escape" || e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return;
      if (isTextField(document.activeElement) || document.querySelector(CLOSABLE)) return;
      window.parent.postMessage({ type: LEAVE_MESSAGE }, location.origin);
    },
    true,
  );
}

async function boot(el: HTMLElement): Promise<void> {
  // The demo is the whole window, so it always draws Strata's own title bar.
  const url = new URL(location.href);
  if (!url.searchParams.has("titlebar")) {
    url.searchParams.set("titlebar", "custom");
    history.replaceState(history.state, "", url);
  }
  embed();
  const r = await fetch(treeUrl);
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  const services = demoServices(await r.arrayBuffer());
  // `?nogl` shows the app's own state for machines without WebGL2.
  if (url.searchParams.has("nogl")) services.createRenderer = () => null;
  const app = useApp.getState();
  // The app applies the saved appearance at startup; the demo has no
  // settings store, so it starts from the engine's defaults (cushion style).
  app.setTreemapStyle("cushion");
  app.openVolume("fixture", 0);
  createRoot(el).render(
    <StrictMode>
      <DemoApp services={services} />
    </StrictMode>,
  );
}

const root = document.getElementById("root");
if (root) {
  boot(root).catch((err: unknown) => {
    console.error("demo failed to start", err);
    root.textContent = "The demo could not load. Reload the page to try again.";
  });
}
