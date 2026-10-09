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
  | "caret";

const PATHS: Record<IconName, ReactElement> = {
  home: <path d="M3 10 10 4l7 6v6a1 1 0 0 1-1 1h-4v-5H8v5H4a1 1 0 0 1-1-1z" />,
  treemap: (
    <>
      <rect x="3" y="3" width="8" height="14" rx="1" />
      <rect x="12" y="3" width="5" height="7" rx="1" />
      <rect x="12" y="11" width="5" height="6" rx="1" />
    </>
  ),
  sunburst: (
    <>
      <circle cx="10" cy="10" r="3" />
      <path d="M10 2a8 8 0 0 1 8 8h-3a5 5 0 0 0-5-5zM18 10a8 8 0 0 1-12.5 6.6l1.7-2.5A5 5 0 0 0 15 10z" />
    </>
  ),
  icicle: (
    <>
      <rect x="3" y="3" width="14" height="4" rx="1" />
      <rect x="3" y="8" width="8" height="4" rx="1" />
      <rect x="12" y="8" width="5" height="4" rx="1" />
      <rect x="3" y="13" width="5" height="4" rx="1" />
    </>
  ),
  flame: (
    <>
      <rect x="3" y="13" width="14" height="4" rx="1" />
      <rect x="3" y="8" width="8" height="4" rx="1" />
      <rect x="12" y="8" width="5" height="4" rx="1" />
      <rect x="3" y="3" width="5" height="4" rx="1" />
    </>
  ),
  bubbles: (
    <>
      <circle cx="8" cy="9" r="5" />
      <circle cx="14.5" cy="13.5" r="3" />
      <circle cx="15" cy="5" r="2" />
    </>
  ),
  mindmap: (
    <>
      <circle cx="10" cy="10" r="2.5" />
      <circle cx="4" cy="4" r="1.8" />
      <circle cx="16" cy="5" r="1.8" />
      <circle cx="15" cy="16" r="1.8" />
      <path d="M8.3 8.3 5.3 5.3M11.9 8.9l2.6-2.6M11.6 11.9l2.3 2.6" strokeWidth="1.4" stroke="currentColor" fill="none" />
    </>
  ),
  search: <path d="M8.5 3a5.5 5.5 0 0 1 4.4 8.8l3.7 3.7-1.1 1.1-3.7-3.7A5.5 5.5 0 1 1 8.5 3zm0 1.5a4 4 0 1 0 0 8 4 4 0 0 0 0-8z" />,
  list: <path d="M3 4h14v2H3zM3 9h14v2H3zM3 14h14v2H3z" />,
  detail: <path d="M3 3h14v14H3zm9 1.5v11h3.5v-11z" fillRule="evenodd" />,
  nav: <path d="M3 3h14v14H3zm1.5 1.5v11H7v-11z" fillRule="evenodd" />,
  chevron: <path d="m7 4 6 6-6 6-1.1-1.1L10.8 10 5.9 5.1z" />,
  caret: <path d="m5 7 5 6 5-6z" />,
  folder: <path d="M2 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1z" />,
  file: <path d="M5 2h7l4 4v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1zm6 1.5V7h3.5z" />,
  close: <path d="m5.1 4 4.9 4.9L14.9 4 16 5.1 11.1 10l4.9 4.9-1.1 1.1-4.9-4.9L5.1 16 4 14.9 8.9 10 4 5.1z" />,
  warning: <path d="M10 2 19 18H1zm-.8 5v6h1.6V7zm0 7.5V16h1.6v-1.5z" fillRule="evenodd" />,
  shield: <path d="M10 2 17 5v5c0 4-3 7-7 8-4-1-7-4-7-8V5z" />,
  lock: <path d="M6 8V6a4 4 0 1 1 8 0v2h1a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V9a1 1 0 0 1 1-1zm1.5 0h5V6a2.5 2.5 0 0 0-5 0z" fillRule="evenodd" />,
};

/** Props for {@link Icon}. */
export interface IconProps {
  name: IconName;
  /** Pixel size (square). */
  size?: number;
  className?: string;
}

/** A decorative 20×20 icon in `currentColor`. */
export function Icon({ name, size = 16, className }: IconProps) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 20 20"
      fill="currentColor"
      aria-hidden="true"
      focusable="false"
    >
      {PATHS[name]}
    </svg>
  );
}
