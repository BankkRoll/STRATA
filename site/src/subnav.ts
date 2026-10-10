/**
 * The in-page section bar on long pages (`[data-subnav]`): marks the section
 * in view with `aria-current="location"` and keeps that link visible when the
 * bar scrolls sideways on narrow screens.
 */

/** Wires the section bar, if the page has one. */
export function subnav(): void {
  const bar = document.querySelector<HTMLElement>("[data-subnav]");
  const list = bar?.querySelector<HTMLElement>("ul");
  if (!bar || !list || typeof IntersectionObserver === "undefined") return;
  const links = new Map<string, HTMLAnchorElement>();
  for (const a of bar.querySelectorAll<HTMLAnchorElement>('a[href^="#"]')) links.set(a.hash.slice(1), a);
  const sections = [...links.keys()].map((id) => document.getElementById(id)).filter((s): s is HTMLElement => s !== null);

  let current: HTMLAnchorElement | undefined;
  const mark = (id: string | null) => {
    const link = id ? links.get(id) : undefined;
    if (link === current) return;
    current?.removeAttribute("aria-current");
    current = link;
    if (!link) return;
    link.setAttribute("aria-current", "location");
    // NOTE: scrolls only the bar; scrollIntoView would also move the page.
    const left = link.offsetLeft - (list.clientWidth - link.offsetWidth) / 2;
    list.scrollTo({ left, behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
  };

  const inView = new Set<string>();
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (e.isIntersecting) inView.add(e.target.id);
        else inView.delete(e.target.id);
      }
      mark(sections.find((s) => inView.has(s.id))?.id ?? null);
    },
    { rootMargin: "-30% 0px -60% 0px" },
  );
  for (const s of sections) io.observe(s);
}
