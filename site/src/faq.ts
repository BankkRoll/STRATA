/**
 * The FAQ page (`faq/index.html`): a filter over the questions, expand and
 * collapse all, and deep links. Each answer is a native `<details>` with the
 * same anchor GitHub gives the question in `docs/FAQ.md`.
 *
 * Without scripts the accordion still works and anchors still scroll; the
 * filter only appears once this runs.
 */

/** At most this many matches are opened automatically while filtering. */
const AUTO_OPEN = 3;

const normalize = (s: string): string => s.toLowerCase().normalize("NFKD").replace(/[̀-ͯ]/g, "");

/** Wires the FAQ page, if this is it. */
export function faq(): void {
  const list = document.querySelector<HTMLElement>("[data-faq]");
  if (!list) return;
  const items = [...list.querySelectorAll<HTMLDetailsElement>("[data-qa]")];
  const tools = document.querySelector<HTMLElement>("[data-faq-tools]");
  const input = document.querySelector<HTMLInputElement>("[data-faq-filter]");
  const toggleAll = document.querySelector<HTMLButtonElement>("[data-faq-toggle]");
  const status = document.querySelector<HTMLElement>("[data-faq-status]");
  const empty = document.querySelector<HTMLElement>("[data-faq-empty]");
  if (tools) tools.hidden = false;

  const visible = () => items.filter((d) => !d.hidden);
  const syncToggle = () => {
    if (toggleAll) toggleAll.textContent = visible().every((d) => d.open) ? "Collapse all" : "Expand all";
  };

  const filter = () => {
    const words = normalize(input?.value ?? "").split(/\s+/).filter(Boolean);
    let shown = 0;
    for (const d of items) {
      const hit = words.every((w) => normalize(d.dataset.text ?? "").includes(w));
      d.hidden = !hit;
      if (hit) shown++;
    }
    if (words.length > 0 && shown <= AUTO_OPEN) for (const d of visible()) d.open = true;
    if (empty) empty.hidden = shown > 0;
    if (status) status.textContent = words.length ? `${shown} of ${items.length} questions` : "";
    syncToggle();
  };
  input?.addEventListener("input", filter);
  input?.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && input.value) {
      e.preventDefault();
      input.value = "";
      filter();
    }
  });

  toggleAll?.addEventListener("click", () => {
    const shown = visible();
    const open = !shown.every((d) => d.open);
    for (const d of shown) d.open = open;
    syncToggle();
  });

  for (const d of items) {
    d.addEventListener("toggle", () => {
      // NOTE: replaceState keeps the address shareable without adding a history entry per click.
      if (d.open) history.replaceState(null, "", `#${d.id}`);
      else if (location.hash === `#${d.id}`) history.replaceState(null, "", location.pathname + location.search);
      syncToggle();
    });
    const link = d.querySelector<HTMLAnchorElement>("[data-qa-link]");
    link?.addEventListener("click", (e) => {
      e.preventDefault();
      const url = `${location.origin}${location.pathname}#${d.id}`;
      history.replaceState(null, "", `#${d.id}`);
      const done = (text: string) => {
        link.textContent = text;
        setTimeout(() => {
          link.textContent = "Link to this answer";
        }, 1600);
      };
      if (!navigator.clipboard) return;
      navigator.clipboard.writeText(url).then(
        () => {
          done("Link copied");
        },
        () => undefined,
      );
    });
  }

  const openFromHash = () => {
    const id = decodeURIComponent(location.hash.slice(1));
    const target = id ? document.getElementById(id) : null;
    if (!(target instanceof HTMLDetailsElement) || !items.includes(target)) return;
    target.hidden = false;
    target.open = true;
    target.scrollIntoView({ block: "start" });
    target.querySelector("summary")?.focus({ preventScroll: true });
  };
  window.addEventListener("hashchange", openFromHash);
  openFromHash();
}
