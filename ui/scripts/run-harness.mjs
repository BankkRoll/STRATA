// Drives the dev-only fixture harness in a Chromium browser (Edge/WebView2's
// engine) over the DevTools protocol: checks shader compilation and records
// frame-time benchmarks.
//
//   pnpm --dir ui dev            # in another terminal
//   node ui/scripts/run-harness.mjs [small|large] [view]
//
// BROWSER overrides the browser executable (default: Microsoft Edge).
// SCREENSHOT=<file.png> saves a screenshot after loading; BENCH=0 skips benchmarks;
// WINDOW=900,600 sets the window size.
// Headless mode still uses the GPU on Windows (ANGLE/D3D11); the GPU name is
// printed so results can be compared across machines.
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const fixture = process.argv[2] ?? "small";
const view = process.argv[3] ?? "treemap";
const port = 9333;
const browser = process.env.BROWSER ?? "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe";
const profile = mkdtempSync(join(tmpdir(), "strata-harness-"));
const proc = spawn(
  browser,
  [
    "--headless=new",
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    `--window-size=${process.env.WINDOW ?? "1600,1000"}`,
    "--ignore-gpu-blocklist",
    "--use-angle=d3d11",
    "--disable-gpu-vsync-throttling",
    "about:blank",
  ],
  { stdio: "ignore" },
);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function target() {
  for (let i = 0; i < 50; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = list.find((t) => t.type === "page");
      if (page) return page.webSocketDebuggerUrl;
    } catch {
      // Browser still starting.
    }
    await sleep(200);
  }
  throw new Error("browser did not expose a page target");
}

let id = 0;
const pending = new Map();
const ws = new WebSocket(await target());
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
ws.addEventListener("message", (e) => {
  const msg = JSON.parse(String(e.data));
  if (msg.method === "Runtime.consoleAPICalled") {
    console.log("[page]", msg.params.args.map((a) => a.value ?? a.description).join(" "));
  }
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  }
});
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const n = ++id;
    pending.set(n, resolve);
    ws.send(JSON.stringify({ id: n, method, params }));
  });
const evaluate = async (expression) => {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.exception?.description ?? "evaluation failed");
  return r.result?.result?.value;
};

try {
  await send("Runtime.enable");
  await send("Page.enable");
  const url = `http://localhost:5173/?fixture=${fixture}${view === "treemap" ? "" : `&view=${view}`}`;
  await send("Page.navigate", { url });
  for (let i = 0; i < 150; i++) {
    if (await evaluate("Boolean(window.__strataHarness?.ready && window.__strataView?.currentFrame)")) break;
    await sleep(200);
  }
  const gpu = await evaluate(`(() => {
    const gl = document.createElement("canvas").getContext("webgl2");
    const ext = gl && gl.getExtension("WEBGL_debug_renderer_info");
    return gl ? (ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER)) : "no webgl2";
  })()`);
  const shaders = await evaluate("window.__strataHarness.shaders");
  const frameLoadMs = await evaluate("window.__strataView.lastFrameLoadMs");
  const instances = await evaluate("window.__strataView.currentFrame.nodes.count");
  console.log(JSON.stringify({ fixture, view, gpu, shaders, instances, frameLoadMs }));
  if (process.env.SCREENSHOT) {
    await sleep(800);
    const shot = await send("Page.captureScreenshot", { format: "png" });
    writeFileSync(process.env.SCREENSHOT, Buffer.from(shot.result.data, "base64"));
  }
  for (const name of process.env.BENCH === "0" ? [] : ["hover", "zoom", "pan"]) {
    const r = await evaluate(`window.__strataHarness.run(${JSON.stringify(name)}, 5000)`);
    console.log(JSON.stringify(r));
  }
} finally {
  ws.close();
  proc.kill();
  await sleep(500);
  rmSync(profile, { recursive: true, force: true });
}
