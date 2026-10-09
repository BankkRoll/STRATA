/**
 * Inline SVG icons (no icon font, no network). All are decorative: callers
 * label the control, and every icon is `aria-hidden`.
 */
import type { ReactElement } from "react";

/** Icon names. */
export type IconName =
  | "home"
  | "treemap"
  | "sunburst"
  | "icicle"
  | "flame"
  | "bubbles"
  | "mindmap"
  | "search"
  | "list"
  | "detail"
  | "nav"
  | "chevron"
  | "folder"
  | "file"
  | "close"
  | "warning"
  | "shield"
  | "lock"
  | "caret"
  | "info"
  | "broom"
  | "tools"
  | "gear"
  | "bars"
  | "history"
  | "pulse"
  | "copies"
  | "apps"
  | "tag"
  | "sparkle"
  | "largest"
  | "check";

// Drawn on a 20×20 grid as 1.5px strokes; coordinates sit on half pixels so
// lines stay crisp at 16 and 20 px.
const PATHS: Record<IconName, ReactElement> = {
  home: <path d="M3.75 9 10 3.75 16.25 9v6.5a.75.75 0 0 1-.75.75H12.5v-4.5h-5v4.5H4.5a.75.75 0 0 1-.75-.75z" />,
  treemap: (
    <>
      <rect x="3.25" y="3.25" width="13.5" height="13.5" rx="1" />
      <path d="M10.75 3.25v13.5M10.75 9.75h6" />
    </>
  ),
  sunburst: (
    <>
      <circle cx="10" cy="10" r="2.75" />
      <circle cx="10" cy="10" r="6.75" />
      <path d="M10 3.25v4M16.75 10h-4M5.25 14.75l2.8-2.8" />
    </>
  ),
  icicle: (
    <>
      <rect x="3.25" y="3.25" width="13.5" height="13.5" rx="1" />
      <path d="M3.25 7.75h13.5M3.25 12.25h7.5M10.75 7.75v9M7.25 12.25v4.5" />
    </>
  ),
  flame: (
    <>
      <rect x="3.25" y="3.25" width="13.5" height="13.5" rx="1" />
      <path d="M3.25 12.25h13.5M3.25 7.75h7.5M10.75 3.25v9M7.25 3.25v4.5" />
    </>
  ),
  bubbles: (
    <>
      <circle cx="8" cy="9" r="4.75" />
      <circle cx="14.5" cy="14" r="2.5" />
      <circle cx="15" cy="5" r="1.75" />
    </>
  ),
  mindmap: (
    <>
      <circle cx="10" cy="10" r="2.25" />
      <circle cx="4.5" cy="4.5" r="1.5" />
      <circle cx="15.5" cy="5" r="1.5" />
      <circle cx="15" cy="15.5" r="1.5" />
      <path d="M8.4 8.4 5.6 5.6M11.7 8.6l2.7-2.5M11.6 11.7l2.4 2.7" />
    </>
  ),
  search: <path d="M8.75 3.25a5.5 5.5 0 1 1 0 11 5.5 5.5 0 0 1 0-11zM12.75 12.75l4 4" />,
  list: <path d="M3.75 5h12.5M3.75 10h12.5M3.75 15h12.5" />,
  detail: (
    <>
      <rect x="3.25" y="3.25" width="13.5" height="13.5" rx="1" />
      <path d="M12.25 3.25v13.5" />
    </>
  ),
  nav: (
    <>
      <rect x="3.25" y="3.25" width="13.5" height="13.5" rx="1" />
      <path d="M7.75 3.25v13.5" />
    </>
  ),
  chevron: <path d="m7.75 4.75 5.25 5.25-5.25 5.25" />,
  caret: <path d="m6 8 4 4 4-4" />,
  folder: <path d="M2.75 5.5a.75.75 0 0 1 .75-.75h4.25l2 2h6.75a.75.75 0 0 1 .75.75v7.5a.75.75 0 0 1-.75.75h-13a.75.75 0 0 1-.75-.75z" />,
  file: <path d="M5.5 2.75h6l3.75 3.75v10a.75.75 0 0 1-.75.75h-9a.75.75 0 0 1-.75-.75V3.5a.75.75 0 0 1 .75-.75zM11.25 2.75v4h4" />,
  close: <path d="m5 5 10 10M15 5 5 15" />,
  warning: <path d="M10 3.25 17.5 16.5h-15zM10 8v4M10 14.25v.25" />,
  shield: <path d="M10 2.75 16.25 5.25v4.5c0 3.75-2.75 6.5-6.25 7.5-3.5-1-6.25-3.75-6.25-7.5v-4.5z" />,
  info: (
    <>
      <circle cx="10" cy="10" r="7.25" />
      <path d="M10 9v5M10 6.25v.25" />
    </>
  ),
  broom: <path d="m16.25 3.75-5.5 5.5M8.25 8.75l3 3-1 5.5h-6.5l1-4.75zM6.5 17.25l.75-3.5M9.5 9.75l2-2 1 1-2 2" />,
  tools: <path d="M13.25 2.75a4 4 0 0 0-3.8 5.25L3.2 14.25a1.4 1.4 0 0 0 2 2L11.5 10a4 4 0 0 0 5.25-3.8l-2.25 2.25-2.25-.75-.75-2.25z" />,
  gear: (
    <>
      <circle cx="10" cy="10" r="2.5" />
      <path d="M8.75 2.75h2.5l.4 2 1.35.6 1.7-1.15 1.8 1.8-1.15 1.7.6 1.35 2 .4v2.5l-2 .4-.6 1.35 1.15 1.7-1.8 1.8-1.7-1.15-1.35.6-.4 2h-2.5l-.4-2-1.35-.6-1.7 1.15-1.8-1.8 1.15-1.7-.6-1.35-2-.4v-2.5l2-.4.6-1.35-1.15-1.7 1.8-1.8 1.7 1.15 1.35-.6z" />
    </>
  ),
  bars: <path d="M3.75 4.75h8.5M3.75 10h12.5M3.75 15.25h5" />,
  history: <path d="M3.5 10a6.5 6.5 0 1 0 1.9-4.6M3.25 3.5v3h3M10 6.5V10l2.5 2.5" />,
  pulse: <path d="M2 10h3.25l2-5 3.5 10 2.25-6 1.25 1h3.75" />,
  copies: (
    <>
      <rect x="6.75" y="2.75" width="9.5" height="11.5" rx="1" />
      <path d="M4.25 6.25v10.25a.75.75 0 0 0 .75.75h7.75" />
    </>
  ),
  apps: (
    <>
      <rect x="3.25" y="3.25" width="5.5" height="5.5" rx="1" />
      <rect x="11.25" y="3.25" width="5.5" height="5.5" rx="1" />
      <rect x="3.25" y="11.25" width="5.5" height="5.5" rx="1" />
      <rect x="11.25" y="11.25" width="5.5" height="5.5" rx="1" />
    </>
  ),
  tag: (
    <>
      <path d="M2.75 3.5a.75.75 0 0 1 .75-.75h5.75l8 8-6.5 6.5-8-8z" />
      <circle cx="6.5" cy="6.5" r="1.25" />
    </>
  ),
  sparkle: <path d="M10 2.75 11.6 8.4l5.65 1.6-5.65 1.6L10 17.25 8.4 11.6 2.75 10 8.4 8.4z" />,
  largest: <path d="M3.75 4.75h12.5M3.75 10h8.5M3.75 15.25h5" />,
  check: <path d="m4 10.5 3.75 3.75L16 6" />,
  lock: (
    <>
      <rect x="4.25" y="8.75" width="11.5" height="8.5" rx="1" />
      <path d="M7 8.75V6.5a3 3 0 0 1 6 0v2.25" />
    </>
  ),
};

/** Props for {@link Icon}. */
export interface IconProps {
  name: IconName;
  /** Pixel size (square). */
  size?: number;
  className?: string;
}

/** A decorative 20×20 line icon stroked in `currentColor`. */
export function Icon({ name, size = 16, className }: IconProps) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {PATHS[name]}
    </svg>
  );
}
