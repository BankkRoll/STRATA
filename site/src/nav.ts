/**
 * The top navigation: desktop mega menus and the mobile sheet.
 *
 * Responsibilities:
 * - Mega menus follow the disclosure pattern: each trigger is a button with
 *   `aria-expanded` and `aria-controls`, and its panel follows it in the DOM,
 *   so Tab moves from a trigger straight into its links.
 * - A shared background box morphs its width, height and position between
 *   panels while their content cross-fades, and a hover indicator slides
 *   between the top-level items.
 * - Pointer hover opens and closes with a short intent delay; click, Enter,
 *   Space and ArrowDown work the same without hover. Escape closes and returns
 *   focus, as do focus or a click leaving the navigation.
 * - Below 960 px a menu button opens a full-height sheet, with page scrolling
 *   locked and the rest of the page inert while it is open.
 *
 * Motion is CSS transitions only, so `prefers-reduced-motion` (handled in the
 * stylesheet) turns all of it off.
 */

/** Delay before a hovered trigger opens its panel, so passing over it does nothing. */
const OPEN_DELAY = 90;
/** Delay before leaving the menu closes it, so a diagonal move to the panel survives. */
const CLOSE_DELAY = 220;
/** Panels never come closer than this to the window edge. */
const EDGE = 16;
const DESKTOP = "(min-width: 960px)";

/** Restarts transitions from the current computed style. */
const reflow = (el: HTMLElement): void => {
  void el.offsetWidth;
};

function megaMenus(): void {
  const nav = document.querySelector<HTMLElement>("[data-nav]");
  const bg = nav?.querySelector<HTMLElement>("[data-nav-bg]");
  const hover = nav?.querySelector<HTMLElement>("[data-nav-hover]");
  if (!nav || !bg || !hover) return;
  const triggers = [...nav.querySelectorAll<HTMLButtonElement>("[data-nav-trigger]")];
  const items = [...nav.querySelectorAll<HTMLElement>(".nav__trigger, .nav__link")];
  const panelOf = (t: HTMLButtonElement) => document.getElementById(t.getAttribute("aria-controls") ?? "");

  let open: HTMLButtonElement | null = null;
  let openedByHover = false;
  let openTimer: ReturnType<typeof setTimeout> | undefined;
  let closeTimer: ReturnType<typeof setTimeout> | undefined;

  const cancelTimers = () => {
    clearTimeout(openTimer);
    clearTimeout(closeTimer);
  };

  /** Positions a panel under its trigger, kept inside the window, and fits the background to it. */
  const place = (t: HTMLButtonElement, panel: HTMLElement) => {
    const navBox = nav.getBoundingClientRect();
    const tBox = t.getBoundingClientRect();
    const w = panel.offsetWidth;
    const vw = document.documentElement.clientWidth;
    let x = tBox.left - EDGE - navBox.left;
    x = Math.min(x, vw - EDGE - w - navBox.left);
    x = Math.max(x, EDGE - navBox.left);
    panel.style.left = `${x}px`;
    bg.style.width = `${w}px`;
    bg.style.height = `${panel.offsetHeight}px`;
    bg.style.transform = `translateX(${x}px)`;
  };

  /** Moves the hover indicator behind an item; it appears in place rather than sliding in from elsewhere. */
  const indicate = (el: HTMLElement | null) => {
    if (!el) {
      hover.classList.remove("is-on");
      return;
    }
    const navBox = nav.getBoundingClientRect();
    const box = el.getBoundingClientRect();
    const appearing = !hover.classList.contains("is-on");
    if (appearing) hover.classList.add("is-instant");
    hover.style.width = `${box.width}px`;
    hover.style.transform = `translateX(${box.left - navBox.left}px)`;
    if (appearing) {
      reflow(hover);
      hover.classList.remove("is-instant");
    }
    hover.classList.add("is-on");
  };

  const show = (t: HTMLButtonElement, byHover: boolean) => {
    cancelTimers();
    if (open === t) return;
    const panel = panelOf(t);
    if (!panel) return;
    const prev = open;
    const dir = prev ? Math.sign(triggers.indexOf(t) - triggers.indexOf(prev)) : 0;
    if (prev) {
      const out = panelOf(prev);
      prev.setAttribute("aria-expanded", "false");
      out?.style.setProperty("--from", `${-dir * 24}px`);
      out?.classList.remove("is-open");
    } else {
      bg.classList.add("is-instant");
    }
    panel.style.setProperty("--from", `${dir * 24}px`);
    place(t, panel);
    reflow(panel);
    if (!prev) {
      reflow(bg);
      bg.classList.remove("is-instant");
    }
    t.setAttribute("aria-expanded", "true");
    panel.classList.add("is-open");
    nav.classList.add("is-open");
    open = t;
    openedByHover = byHover;
    indicate(t);
  };

  const close = (focusTrigger = false) => {
    cancelTimers();
    if (!open) return;
    const t = open;
    const panel = panelOf(t);
    t.setAttribute("aria-expanded", "false");
    panel?.style.setProperty("--from", "0px");
    panel?.classList.remove("is-open");
    nav.classList.remove("is-open");
    open = null;
    if (focusTrigger) t.focus();
    if (!nav.matches(":hover")) indicate(null);
  };

  const scheduleClose = () => {
    clearTimeout(openTimer);
    clearTimeout(closeTimer);
    closeTimer = setTimeout(() => {
      close();
    }, CLOSE_DELAY);
  };

  for (const t of triggers) {
    t.addEventListener("pointerenter", (e) => {
      if (e.pointerType === "touch") return;
      clearTimeout(closeTimer);
      if (open) show(t, true);
      else {
        clearTimeout(openTimer);
        openTimer = setTimeout(() => {
          show(t, true);
        }, OPEN_DELAY);
      }
    });
    t.addEventListener("pointerleave", (e) => {
      if (e.pointerType !== "touch") scheduleClose();
    });
    t.addEventListener("click", () => {
      // NOTE: a hover may have opened the panel a moment before the click; keep it open rather than toggle it shut.
      if (open === t && openedByHover) openedByHover = false;
      else if (open === t) close();
      else show(t, false);
    });
    t.addEventListener("keydown", (e) => {
      if (e.key !== "ArrowDown") return;
      e.preventDefault();
      show(t, false);
      panelOf(t)?.querySelector<HTMLElement>("a, button")?.focus();
    });
    const panel = panelOf(t);
    panel?.addEventListener("pointerenter", () => {
      clearTimeout(closeTimer);
    });
    panel?.addEventListener("pointerleave", (e) => {
      if (e.pointerType !== "touch") scheduleClose();
    });
  }

  for (const item of items) {
    item.addEventListener("pointerenter", (e) => {
      if (e.pointerType !== "touch") indicate(item);
    });
    item.addEventListener("focus", () => {
      if (item.matches(":focus-visible")) indicate(item);
    });
  }
  nav.querySelector(".nav__list")?.addEventListener("pointerleave", () => {
    indicate(open);
  });
  nav.addEventListener("focusout", (e) => {
    const next = e.relatedTarget;
    if (next instanceof Node && nav.contains(next)) return;
    close();
    indicate(null);
  });
  nav.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && open) {
      e.preventDefault();
      close(true);
    }
  });
  document.addEventListener("pointerdown", (e) => {
    if (open && e.target instanceof Node && !nav.contains(e.target)) close();
  });
  window.addEventListener("resize", () => {
    if (!open) return;
    if (!window.matchMedia(DESKTOP).matches) close();
    else {
      const panel = panelOf(open);
      if (panel) place(open, panel);
    }
  });
}

function mobileSheet(): void {
  const toggle = document.querySelector<HTMLButtonElement>("[data-sheet-toggle]");
  const sheet = document.querySelector<HTMLElement>("[data-sheet]");
  if (!toggle || !sheet) return;
  const outside = () => document.querySelectorAll<HTMLElement>("main, footer, .skip");

  const set = (on: boolean, returnFocus = false) => {
    toggle.setAttribute("aria-expanded", String(on));
    sheet.classList.toggle("is-open", on);
    document.documentElement.classList.toggle("is-locked", on);
    for (const el of outside()) el.inert = on;
    if (on) sheet.querySelector<HTMLElement>("a")?.focus({ preventScroll: true });
    else if (returnFocus) toggle.focus();
  };

  toggle.addEventListener("click", () => {
    set(toggle.getAttribute("aria-expanded") !== "true");
  });
  sheet.addEventListener("click", (e) => {
    if (e.target instanceof Element && e.target.closest("a")) set(false);
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && sheet.classList.contains("is-open")) set(false, true);
  });
  window.matchMedia(DESKTOP).addEventListener("change", (e) => {
    if (e.matches) set(false);
  });
}

/** Wires the top navigation on every page. */
export function navigation(): void {
  const header = document.querySelector<HTMLElement>("[data-topnav]");
  if (header) {
    const update = () => header.classList.toggle("is-scrolled", window.scrollY > 8);
    update();
    window.addEventListener("scroll", update, { passive: true });
  }
  megaMenus();
  mobileSheet();
}
