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
  info: <path d="M10 2a8 8 0 1 1 0 16 8 8 0 0 1 0-16zm0 1.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13zM9.2 9h1.6v5H9.2zm0-3h1.6v1.6H9.2z" fillRule="evenodd" />,
  broom: <path d="M15.6 2.3 17 3.7l-5 5 1.4 1.4-1.1 1.1L9 7.9l1.1-1.1 1.4 1.4zM8 9l3 3-1 6H3l1-5z" />,
  tools: <path d="M13.5 2a4.5 4.5 0 0 0-4.3 5.8L2.6 14.4a1.5 1.5 0 0 0 2.1 2.1l6.6-6.6A4.5 4.5 0 0 0 17.7 5l-2.6 2.6-2-.6-.6-2L15.1 2.4A4.5 4.5 0 0 0 13.5 2z" />,
  gear: <path d="m8.6 2h2.8l.4 2.2 1.4.6 1.9-1.3 2 2-1.3 1.9.6 1.4 2.2.4v2.8l-2.2.4-.6 1.4 1.3 1.9-2 2-1.9-1.3-1.4.6-.4 2.2H8.6l-.4-2.2-1.4-.6-1.9 1.3-2-2 1.3-1.9-.6-1.4L1.4 11.4V8.6l2.2-.4.6-1.4-1.3-1.9 2-2 1.9 1.3 1.4-.6zM10 7a3 3 0 1 0 0 6 3 3 0 0 0 0-6z" fillRule="evenodd" />,
  bars: <path d="M3 3h10v3H3zM3 8.5h14v3H3zM3 14h6v3H3z" />,
  history: <path d="M10 2a8 8 0 1 1-7.4 5h1.7A6.5 6.5 0 1 0 6 4.8L7.5 6.3H2.5v-5l2.4 2.4A8 8 0 0 1 10 2zm-.8 4h1.6v3.7l2.7 2.7-1.1 1.1-3.2-3.2z" />,
  pulse: <path d="M1 10h4l2-5 3 10 2.5-7 1.5 2h5v1.5h-5.8l-.6-.8-2.6 7.3L7 7.7 6 10.8l-.3.7H1z" />,
  copies: <path d="M7 2h7l3 3v9a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1zM3 6h1.5v10.5H13V18H4a1 1 0 0 1-1-1z" />,
  apps: <path d="M3 3h6v6H3zm8 0h6v6h-6zM3 11h6v6H3zm8 0h6v6h-6z" />,
  tag: <path d="M2 3a1 1 0 0 1 1-1h6l9 9-7 7-9-9zm4 1.5a1.5 1.5 0 1 0 0 3 1.5 1.5 0 0 0 0-3z" fillRule="evenodd" />,
  sparkle: <path d="m10 1 2 7 7 2-7 2-2 7-2-7-7-2 7-2z" />,
  largest: <path d="M3 3h14v4H3zm0 6h10v4H3zm0 6h6v3H3z" />,
  check: <path d="m8 13.2 7.6-7.6 1.1 1.1L8 15.4 3.3 10.7l1.1-1.1z" />,
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
