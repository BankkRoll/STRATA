/**
 * Page bootstrap: scroll reveals, the architecture captions, copy buttons on
 * the benchmark reproduction commands, and the lazy demo. The demo module (renderer,
 * fixtures) loads only when its section nears the viewport, so the first paint costs a few kilobytes of script.
 */
import "virtual:strata-app.css";
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

function demo(): void {
  const root = document.querySelector<HTMLElement>("[data-demo]");
  if (!root) return;
  const start = () => {
    import("./demo/demo")
      .then(async ({ mountDemo }) => {
        const handle = await mountDemo(root);
        // Zoom out of the drill folder once most of the window is visible.
        const io = new IntersectionObserver(
          (entries) => {
            if (!entries.some((e) => e.isIntersecting)) return;
            io.disconnect();
            setTimeout(() => {
              handle.playIntro();
            }, 350);
          },
          { threshold: 0.55 },
        );
        io.observe(root);
      })
      .catch((err: unknown) => {
        console.error(err);
        const state = root.querySelector<HTMLElement>("[data-state]");
        if (state) {
          state.className = "state state--error";
          state.innerHTML = "<h2>The demo could not load</h2><p>Reload the page to try again.</p>";
        }
      });
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

reveals();
topnav();
architecture();
copyButtons();
demo();
