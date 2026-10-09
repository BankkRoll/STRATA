/**
 * F6 region cycling. Every major region of the shell carries a
 * `data-region` attribute; F6 / Shift+F6 move focus to the next / previous
 * visible region in document order, landing on the element the region marks
 * as its entry point (`data-region-focus`), else its first focusable child,
 * else the region itself.
 */

const FOCUSABLE = "button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])";

function entryPoint(region: HTMLElement): HTMLElement {
  const marked = region.querySelector<HTMLElement>("[data-region-focus]");
  if (marked) return marked;
  const active = region.querySelector<HTMLElement>("[aria-selected='true'][tabindex='0'], [aria-current='page']");
  if (active) return active;
  return region.querySelector<HTMLElement>(FOCUSABLE) ?? region;
}

/**
 * Moves focus to the next or previous region.
 *
 * @param dir - `1` forward, `-1` backward.
 * @param root - Where to look for regions.
 * @returns The region name that received focus, or `null` when there is none.
 */
export function cycleRegion(dir: 1 | -1, root: ParentNode = document): string | null {
  const regions = [...root.querySelectorAll<HTMLElement>("[data-region]")].filter((r) => !r.closest("[hidden], [inert]"));
  if (regions.length === 0) return null;
  const active = document.activeElement;
  const cur = regions.findIndex((r) => r.contains(active));
  const next = regions[cur < 0 ? (dir === 1 ? 0 : regions.length - 1) : (cur + dir + regions.length) % regions.length] as HTMLElement;
  const target = entryPoint(next);
  if (target === next && !next.hasAttribute("tabindex")) next.tabIndex = -1;
  target.focus();
  return next.dataset.region ?? null;
}
