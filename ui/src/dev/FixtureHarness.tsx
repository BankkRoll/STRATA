/**
 * DEV ONLY harness (`/?fixture` or `/?fixture=large`): runs the real app shell
 * over exported layout fixtures, checks that every shader compiles, and
 * measures frame times. Excluded from production builds (see `main.tsx`).
 *
 * Results are also exposed on `window.__strataHarness` so a headless browser
 * can collect them.
 */
import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { App } from "../App";
import { loadRenderer, createRenderer } from "../render/registry";
import type { ViewController } from "../render/ViewController";
import { useApp } from "../store/app";
import { FIXTURE_KINDS, fixtureServices, loadFixtures } from "./fixtureServices";

/** Frame-time statistics of one benchmark run. */
export interface BenchResult {
  name: string;
  instances: number;
  frames: number;
  fps: number;
  meanMs: number;
  p50Ms: number;
  p95Ms: number;
  p99Ms: number;
  maxMs: number;
  /** Mean CPU time spent in the draw call (GPU work is asynchronous). */
  drawCpuMs: number;
  /** Mean CPU time of label passes that ran. */
  labelMs: number;
  /** Mean hit-test time per query. */
  pickUs: number;
}

interface HarnessState {
  shaders: Record<string, string>;
  bench: BenchResult[];
  ready: boolean;
}

declare global {
  interface Window {
    __strataView?: ViewController;
    __strataHarness?: HarnessState & { run: (name: "hover" | "zoom" | "pan", ms?: number) => Promise<BenchResult> };
  }
}

const state: HarnessState = { shaders: {}, bench: [], ready: false };

function checkShaders(): Record<string, string> {
  const out: Record<string, string> = {};
  for (const kind of FIXTURE_KINDS) {
    const canvas = document.createElement("canvas");
    try {
      const r = createRenderer(canvas, kind);
      out[String(kind)] = r ? "ok" : "webgl2 unavailable";
      r?.dispose();
    } catch (err) {
      out[String(kind)] = err instanceof Error ? err.message : String(err);
    }
    canvas.getContext("webgl2")?.getExtension("WEBGL_lose_context")?.loseContext();
  }
  return out;
}

function stats(name: string, times: number[], draw: number[], labels: number[], instances: number, pickUs: number): BenchResult {
  const d = times.slice(1).map((t, i) => t - (times[i] ?? t));
  const sorted = d.slice().sort((a, b) => a - b);
  const q = (p: number) => sorted[Math.min(sorted.length - 1, Math.floor(p * sorted.length))] ?? 0;
  const mean = d.reduce((a, b) => a + b, 0) / Math.max(1, d.length);
  return {
    name,
    instances,
    frames: d.length,
    fps: 1000 / mean,
    meanMs: mean,
    p50Ms: q(0.5),
    p95Ms: q(0.95),
    p99Ms: q(0.99),
    maxMs: sorted[sorted.length - 1] ?? 0,
    drawCpuMs: draw.reduce((a, b) => a + b, 0) / Math.max(1, draw.length),
    labelMs: labels.reduce((a, b) => a + b, 0) / Math.max(1, labels.length),
    pickUs,
  };
}

/**
 * Runs one benchmark: renders every frame for `ms` while changing hover
 * (and the visual zoom / pan), like a user sweeping the mouse.
 */
async function run(name: "hover" | "zoom" | "pan", ms = 5000): Promise<BenchResult> {
  const c = window.__strataView;
  const frame = c?.currentFrame;
  if (!c || !frame) throw new Error("no view mounted yet");
  const n = frame.nodes.count;
  const times: number[] = [];
  const draws: number[] = [];
  const labels: number[] = [];
  let lastLabel = c.lastLabelMs;
  let pickTotal = 0;
  let picks = 0;
  c.continuous = true;
  const start = performance.now();
  await new Promise<void>((resolve) => {
    const tick = (now: number) => {
      const t = (now - start) / ms;
      if (t >= 1) {
        resolve();
        return;
      }
      times.push(now);
      draws.push(c.lastDrawMs);
      if (c.lastLabelMs !== lastLabel) {
        labels.push(c.lastLabelMs);
        lastLabel = c.lastLabelMs;
      }
      const p0 = performance.now();
      const x = ((now * 0.37) % frame.width) | 0;
      const y = ((now * 0.21) % frame.height) | 0;
      const r = c.canvasRect();
      const hit = c.pickAt(r.left + (x / frame.width) * r.width, r.top + (y / frame.height) * r.height);
      pickTotal += performance.now() - p0;
      picks++;
      c.setHoverIndex(hit);
      if (name === "zoom") {
        const s = 1 + 7 * (0.5 - 0.5 * Math.cos(t * Math.PI * 4));
        c.setTransform({ scale: s, tx: frame.width / 2 - (frame.width / 2) * s, ty: frame.height / 2 - (frame.height / 2) * s });
      } else if (name === "pan") {
        const s = 4;
        c.setTransform({ scale: s, tx: -(frame.width * (s - 1)) * (0.5 + 0.5 * Math.sin(t * 6.28)), ty: -(frame.height * (s - 1)) * 0.5 });
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
  c.continuous = false;
  c.setTransform({ scale: 1, tx: 0, ty: 0 });
  const result = stats(name, times, draws, labels, n, (pickTotal / Math.max(1, picks)) * 1000);
  state.bench.push(result);
  console.info("[harness]", JSON.stringify(result));
  return result;
}

function PerfPanel() {
  const [, setTick] = useState(0);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    const t = setInterval(() => {
      setTick((x) => x + 1);
    }, 500);
    return () => {
      clearInterval(t);
    };
  }, []);
  const c = window.__strataView;
  const ft = c?.frameTimes ?? [];
  const recent = ft.slice(-60);
  const fps = recent.length > 1 ? (1000 * (recent.length - 1)) / ((recent[recent.length - 1] ?? 0) - (recent[0] ?? 0)) : 0;
  return (
    <div className="harness" role="region" aria-label="Fixture harness">
      <strong>Fixture harness</strong>
      <span>{c?.currentFrame ? `${c.currentFrame.nodes.count.toLocaleString()} instances` : "no frame"}</span>
      <span>{fps > 0 ? `${fps.toFixed(0)} fps (recent)` : "idle"}</span>
      <span>draw {c ? c.lastDrawMs.toFixed(2) : "–"} ms CPU</span>
      <span>
        shaders:{" "}
        {Object.entries(state.shaders)
          .map(([k, v]) => `${k}:${v}`)
          .join(" ")}
      </span>
      {(["hover", "zoom", "pan"] as const).map((n) => (
        <button
          key={n}
          type="button"
          className="btn btn--small"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            void run(n).finally(() => {
              setBusy(false);
            });
          }}
        >
          Bench {n}
        </button>
      ))}
      <ul>
        {state.bench.map((b, i) => (
          <li key={i}>
            {b.name}: {b.fps.toFixed(1)} fps, p95 {b.p95Ms.toFixed(1)} ms, p99 {b.p99Ms.toFixed(1)} ms, draw {b.drawCpuMs.toFixed(2)} ms, labels {b.labelMs.toFixed(1)} ms, pick {b.pickUs.toFixed(1)} µs
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Loads fixtures and mounts the app with fixture services.
 *
 * @param el - Root element.
 */
export function mountFixtureHarness(el: HTMLElement): void {
  const large = new URLSearchParams(location.search).get("fixture") === "large";
  window.__strataHarness = Object.assign(state, { run });
  void (async () => {
    await Promise.all(FIXTURE_KINDS.map((k) => loadRenderer(k)));
    state.shaders = checkShaders();
    const data = await loadFixtures(large);
    const services = fixtureServices(data);
    useApp.getState().openVolume("fixture", 0);
    const view = new URLSearchParams(location.search).get("view");
    if (view === "sunburst" || view === "icicle" || view === "flame" || view === "bubbles" || view === "mindmap") useApp.getState().setView(view);
    createRoot(el).render(
      <StrictMode>
        <App services={services} />
        <PerfPanel />
      </StrictMode>,
    );
    state.ready = true;
  })().catch((err: unknown) => {
    el.textContent = `Fixture harness failed: ${err instanceof Error ? err.message : String(err)}`;
    console.error(err);
  });
}
