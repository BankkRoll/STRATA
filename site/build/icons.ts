/**
 * The site's icon set, as inline SVG strings. Pages and partials reference an
 * icon with an `<!--icon:NAME-->` placeholder, so the navigation, the landing
 * page and the features page draw each capability with the same glyph.
 *
 * Product and resource glyphs are drawn on a 24-unit grid with 1.5-unit
 * strokes and take their colour from CSS (`.icon` in `nav.css`).
 */

const stroke = (body: string): string =>
  `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">${body}</svg>`;

/** GitHub mark, filled. */
export const GITHUB_ICON =
  '<svg viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z" /></svg>';

/** Download arrow into a tray. */
export const DOWNLOAD_ICON =
  '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M10 3.5v9m0 0-3.5-3.5M10 12.5l3.5-3.5M4.5 16h11" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>';

/** Small right-pointing chevron for "go" links. */
export const ARROW_ICON =
  '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>';

/** Every named icon, by placeholder name. */
export const ICONS: Record<string, string> = {
  // Product
  mft: stroke(
    '<rect x="3.75" y="3.75" width="16.5" height="16.5" rx="2" /><path d="M3.75 8.75h16.5M3.75 13.25h16.5M3.75 17.25h16.5M8.25 3.75v16.5" /><path d="M11.5 11h5.5" stroke-width="2.25" />',
  ),
  live: stroke('<path d="M3 12h3.5l2.25-5.5 4 11 2.5-7.5 1.5 2H21" /><circle cx="21" cy="12" r="0.01" stroke-width="2.5" />'),
  explain: stroke('<path d="M4.25 12.25V5.25a1 1 0 0 1 1-1h7l7.5 7.5-8 8z" /><circle cx="8.5" cy="8.5" r="1.25" /><path d="m11.5 13.5 2.5-2.5M13.25 15.25l1.5-1.5" />'),
  cleanup: stroke('<path d="M12 3.25 5.25 6v5.5c0 4.1 2.8 7.4 6.75 9 3.95-1.6 6.75-4.9 6.75-9V6z" /><path d="m9 12 2.1 2.1L15.25 10" />'),
  search: stroke('<circle cx="10.5" cy="10.5" r="6.25" /><path d="m15.25 15.25 5 5M7.75 10.5h5.5M7.75 8h3" />'),
  history: stroke('<path d="M4 12a8 8 0 1 0 2.35-5.65" /><path d="M4 4v3.5h3.5M12 7.75V12l2.75 2" />'),
  dupes: stroke('<rect x="3.75" y="7.75" width="10.5" height="12.5" rx="1.5" /><path d="M8.25 7.75V5.25a1.5 1.5 0 0 1 1.5-1.5h9a1.5 1.5 0 0 1 1.5 1.5v10.5a1.5 1.5 0 0 1-1.5 1.5h-4.5" /><path d="M6.75 12h4.5M6.75 15h4.5" />'),
  activity: stroke('<path d="M4 20V13M9.33 20V8M14.67 20v-5M20 20V4" /><path d="M2.75 20.25h18.5" />'),
  views: stroke('<rect x="3.75" y="3.75" width="16.5" height="16.5" rx="2" /><path d="M12.75 3.75v16.5M12.75 11.5h7.5M3.75 14.25h9M16.5 11.5v8.75" />'),
  // Resources
  releases: stroke('<path d="M4.75 6.75h14.5M4.75 12h14.5M4.75 17.25h9" /><circle cx="18.25" cy="17.25" r="1.5" />'),
  benchmarks: stroke('<path d="M4 19.25a8 8 0 1 1 16 0" /><path d="m12 19.25 4-6.5" /><path d="M6.25 15h1.5M16.25 15h1.5M12 9.75v1.5" />'),
  faq: stroke('<circle cx="12" cy="12" r="8.25" /><path d="M9.6 9.6a2.45 2.45 0 1 1 3.3 2.3c-.55.2-.9.7-.9 1.3v.55" /><path d="M12 16.6v.05" stroke-width="2" />'),
  security: stroke('<rect x="5" y="10.75" width="14" height="9.5" rx="1.5" /><path d="M8.25 10.75V7.75a3.75 3.75 0 0 1 7.5 0v3" /><path d="M12 14.25v2.5" />'),
  rules: stroke('<path d="M6.25 3.75h8.5l3 3v13.5h-11.5z" /><path d="M14.75 3.75v3h3M9 11h6M9 14h6M9 17h3.5" />'),
  docs: stroke('<path d="M4.25 5.25c2.75-1 5.25-.75 7.75 1v13c-2.5-1.75-5-2-7.75-1zM19.75 5.25c-2.75-1-5.25-.75-7.75 1v13c2.5-1.75 5-2 7.75-1z" />'),
  // UI glyphs
  chevron: '<svg class="chev" viewBox="0 0 12 12" aria-hidden="true"><path d="m3 4.5 3 3 3-3" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>',
  star: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m8 1.75 1.85 3.9 4.25.55-3.1 2.95.8 4.2L8 11.3l-3.8 2.05.8-4.2L1.9 6.2l4.25-.55z" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linejoin="round" /></svg>',
  external: '<svg class="ext" viewBox="0 0 12 12" aria-hidden="true"><path d="M4.5 2.75h4.75V7.5M9.25 2.75 3 9" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" /></svg>',
  menu: '<svg viewBox="0 0 20 20" aria-hidden="true"><path class="burger__a" d="M3.5 7h13" /><path class="burger__b" d="M3.5 13h13" /></svg>',
  github: GITHUB_ICON,
  download: DOWNLOAD_ICON,
  arrow: ARROW_ICON,
};

/**
 * Replaces every `<!--icon:NAME-->` placeholder with its SVG.
 *
 * @param html - Page or partial markup.
 * @returns The markup with icons inlined; unknown names are left untouched.
 */
export function inlineIcons(html: string): string {
  return html.replace(/<!--icon:([\w-]+)-->/g, (m, name: string) => ICONS[name] ?? m);
}
