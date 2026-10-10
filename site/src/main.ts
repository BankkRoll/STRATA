/**
 * Page bootstrap: scroll reveals, the architecture captions, copy buttons on
 * the benchmark reproduction commands, and the demo frame. The demo (the app
 * and its sample data) loads only when its section nears the viewport, so the
 * first paint costs a few kilobytes of script.
 */
import "./styles/site.css";

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

function reveals(): void {
  const items = document.querySelectorAll<HTMLElement>(".reveal");
  if (reducedMotion || typeof IntersectionObserver === "undefined") {
    for (const el of items) el.classList.add("is-in");
    return;
  }
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        e.target.classList.add("is-in");
        io.unobserve(e.target);
      }
    },
    { rootMargin: "0px 0px -8% 0px", threshold: 0.15 },
  );
  for (const el of items) io.observe(el);
}

function topnav(): void {
  const nav = document.querySelector<HTMLElement>("[data-topnav]");
  if (!nav) return;
  const update = () => nav.classList.toggle("is-scrolled", window.scrollY > 8);
  update();
  window.addEventListener("scroll", update, { passive: true });
}

function architecture(): void {
  const arch = document.querySelector<HTMLElement>("[data-arch]");
  const caption = arch?.querySelector<HTMLElement>("[data-arch-caption]");
  if (!arch || !caption) return;
  const initial = caption.textContent;
  const show = (node: Element | null) => {
    for (const n of arch.querySelectorAll(".arch-node.is-active")) n.classList.remove("is-active");
    if (node instanceof HTMLElement && node.dataset.explain) {
      node.classList.add("is-active");
      arch.classList.add("has-active");
      caption.textContent = node.dataset.explain;
    } else {
      arch.classList.remove("has-active");
      caption.textContent = initial;
    }
  };
  for (const node of arch.querySelectorAll(".arch-node")) {
    node.addEventListener("pointerenter", () => {
      show(node);
    });
    node.addEventListener("focus", () => {
      show(node);
    });
    node.addEventListener("pointerleave", () => {
      if (document.activeElement !== node) show(null);
    });
    node.addEventListener("blur", () => {
      show(null);
    });
  }
}

/** Copy buttons on the reproduction commands. */
function copyButtons(): void {
  for (const btn of document.querySelectorAll<HTMLButtonElement>("button[data-copy]")) {
    let timer: ReturnType<typeof setTimeout> | undefined;
    btn.addEventListener("click", () => {
      const text = btn.dataset.copy ?? "";
      const done = (label: string) => {
        btn.textContent = label;
        clearTimeout(timer);
        timer = setTimeout(() => {
          btn.textContent = "Copy";
        }, 1600);
      };
      if (!navigator.clipboard) {
        done("Select to copy");
        return;
      }
      navigator.clipboard.writeText(text).then(
        () => {
          done("Copied");
        },
        () => {
          done("Select to copy");
        },
      );
    });
  }
}

/** Window size the demo renders at; the page scales it to fit. */
const DEMO_WIDTH = 1280;
/** Posted by the demo when the visitor presses Escape to leave it (`ui/src/demo/main.tsx`). */
const DEMO_LEAVE = "strata-demo:leave";
/** Tells the demo the scale the page draws it at, so the map renders at that scale. */
const DEMO_SCALE = "strata-demo:scale";

/**
 * The demo: the app itself, built from `ui/` into `demo/` (see
 * `ui/vite.demo.config.ts`), loaded when its section nears the viewport.
 *
 * NOTE: it runs in a same-origin iframe instead of being mounted into this
 * page. The app owns its document: global styles, theme attributes on
 * `<html>`, the document title and window-level shortcuts. A frame keeps all
 * of that exactly as in the app, keeps it from touching the page, and keeps
 * React out of the page's own script.
 *
 * Below 900 px the window is still shown, scaled down, but inert: the shell
 * is a desktop layout and its targets would be a few pixels wide.
 */
function demo(): void {
  const root = document.querySelector<HTMLElement>("[data-demo]");
  const status = root?.querySelector<HTMLElement>("[data-demo-status]");
  if (!root || !status) return;
  status.textContent = "Loading the demo…";
  let frame: HTMLIFrameElement | null = null;
  const scale = () => Math.min(1, root.clientWidth / DEMO_WIDTH);
  const fit = () => {
    root.style.setProperty("--demo-scale", String(scale()));
    frame?.contentWindow?.postMessage({ type: DEMO_SCALE, scale: scale() }, location.origin);
  };
  fit();
  if (typeof ResizeObserver !== "undefined") new ResizeObserver(fit).observe(root);
  else window.addEventListener("resize", fit);

  const start = () => {
    const narrow = window.matchMedia("(max-width: 899px)");
    const f = document.createElement("iframe");
    frame = f;
    f.className = "demo-window__frame";
    f.title = "Strata demo: the app on a sample volume";
    const query = new URLSearchParams({ scale: String(scale()) });
    // `?nogl` passes through to show the app's no-WebGL state.
    if (new URLSearchParams(location.search).has("nogl")) query.set("nogl", "");
    f.src = `${import.meta.env.BASE_URL}demo/?${query.toString()}`;
    f.addEventListener(
      "load",
      () => {
        status.hidden = true;
        fit();
      },
      { once: true },
    );
    const interactive = () => {
      f.inert = narrow.matches;
      root.classList.toggle("is-static", narrow.matches);
    };
    interactive();
    narrow.addEventListener("change", interactive);
    window.addEventListener("message", (e: MessageEvent<unknown>) => {
      if (e.origin !== location.origin || e.source !== f.contentWindow) return;
      if (typeof e.data === "object" && e.data !== null && (e.data as { type?: unknown }).type === DEMO_LEAVE) {
        document.getElementById("demo-title")?.focus();
      }
    });
    root.append(f);
  };
  if (typeof IntersectionObserver === "undefined") {
    start();
    return;
  }
  const near = new IntersectionObserver(
    (entries) => {
      if (!entries.some((e) => e.isIntersecting)) return;
      near.disconnect();
      start();
    },
    { rootMargin: "400px 0px" },
  );
  near.observe(root);
}

interface LatestRelease {
  tag_name: string;
  published_at: string;
  assets: { name: string; size: number; browser_download_url: string }[];
}

/**
 * The download block (`site/build/releases.ts`): marks the installer that
 * matches this PC, and swaps in a release published after the site was built.
 *
 * NOTE: the architecture comes from User-Agent Client Hints, which only
 * Chromium browsers expose; elsewhere both cards stay equal.
 */
function downloads(): void {
  const block = document.querySelector<HTMLElement>("[data-release]");
  if (!block) return;
  const nav = navigator as Navigator & {
    userAgentData?: { getHighEntropyValues(hints: string[]): Promise<{ architecture?: string; platform?: string }> };
  };
  void nav.userAgentData?.getHighEntropyValues(["architecture"]).then(
    (ua) => {
      if (ua.platform && ua.platform !== "Windows") return;
      const arch = ua.architecture === "arm" ? "arm64" : ua.architecture === "x86" ? "x64" : null;
      if (!arch) return;
      for (const card of block.querySelectorAll<HTMLElement>(".dl-card")) {
        const mine = card.dataset.arch === arch;
        card.classList.toggle("is-recommended", mine);
        card.querySelector<HTMLElement>("[data-recommend]")?.toggleAttribute("hidden", !mine);
        card.querySelector(".btn--primary")?.classList.toggle("is-secondary", !mine);
      }
    },
    () => undefined,
  );

  // NOTE: anonymous API calls are limited to 60 an hour per visitor; on any
  // failure the links baked in at build time stay.
  fetch("https://api.github.com/repos/BankkRoll/STRATA/releases/latest", { headers: { Accept: "application/vnd.github+json" } })
    .then((res) => (res.ok ? (res.json() as Promise<LatestRelease>) : null))
    .then((latest) => {
      if (!latest || latest.tag_name === block.dataset.tag) return;
      const mb = (b: number) => `${(b / (1024 * 1024)).toFixed(1)} MB`;
      for (const link of block.querySelectorAll<HTMLAnchorElement>("a[data-asset]")) {
        const arch = link.dataset.asset ?? "";
        const file = latest.assets.find((a) => a.name.toLowerCase().endsWith(`_${arch}-setup.exe`));
        if (!file) continue;
        link.href = file.browser_download_url;
        const meta = block.querySelector(`[data-asset-meta="${arch}"]`);
        if (meta) meta.textContent = `${file.name} · ${mb(file.size)}`;
      }
      const line = block.querySelector("[data-release-line]");
      const date = new Date(latest.published_at).toLocaleDateString("en-US", { year: "numeric", month: "long", day: "numeric", timeZone: "UTC" });
      if (line) line.textContent = `Version ${latest.tag_name.replace(/^v/, "")} · ${date} · Windows 10 / 11`;
      // The baked checksums belong to the older release.
      block.querySelector("[data-sums]")?.remove();
      block.dataset.tag = latest.tag_name;
    })
    .catch(() => undefined);
}

reveals();
topnav();
architecture();
copyButtons();
demo();
downloads();
