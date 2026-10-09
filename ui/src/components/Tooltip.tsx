/**
 * Rich hover tooltip for the visual views: name, size in both
 * modes, share of the current root, items, category, app, modified and
 * safety. Content re-renders only when the hovered record changes; following
 * the pointer is done imperatively, so mouse moves cost no React work.
 */
import { useEffect, useRef, useSyncExternalStore } from "react";
import { describeTimestamp, formatBytes, formatCount, formatPercent } from "../lib/format";
import { useEntryInfo } from "../lib/hooks";
import type { EntryInfoProvider } from "../lib/entries";
import { NodeFlag } from "../lib/layout/frame";
import { categoryInfo } from "../lib/palette";
import { hoverStore } from "../store/hover";
import { useSettings } from "../store/settings";

const SAFETY_LABEL = { safe: "Safe to remove", probably: "Probably safe", careful: "Careful", never: "Never delete" } as const;

/** Props for {@link Tooltip}. */
export interface TooltipProps {
  provider: EntryInfoProvider;
  sizeMode: "allocated" | "logical";
}

/** Floating tooltip following the pointer. */
export function Tooltip({ provider, sizeMode }: TooltipProps) {
  const target = useSyncExternalStore(hoverStore.subscribe, () => hoverStore.getState().target);
  const units = useSettings((s) => s.units);
  const isAgg = target ? (target.flags & NodeFlag.AGGREGATE) !== 0 : false;
  const info = useEntryInfo(provider, target && !isAgg ? target.id : null);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const place = () => {
      const el = ref.current;
      if (!el) return;
      const { clientX, clientY } = hoverStore.getState();
      const w = el.offsetWidth;
      const h = el.offsetHeight;
      el.style.left = `${clientX + 16 + w > window.innerWidth ? clientX - w - 12 : clientX + 16}px`;
      el.style.top = `${clientY + 18 + h > window.innerHeight ? clientY - h - 12 : clientY + 18}px`;
    };
    place();
    return hoverStore.subscribe(place);
  });

  if (!target) return null;
  const fmt = (b: number) => formatBytes(b, { units });

  let body;
  if (isAgg) {
    const a = target.aggregate;
    body = (
      <>
        <div className="tip__name">{a ? `${formatCount(a.count)} small items` : "Small items"}</div>
        {a && (
          <dl className="tip__grid">
            <dt>Size</dt>
            <dd>{fmt(a.bytes)}</dd>
            <dt>Share</dt>
            <dd>{formatPercent(target.rootBytes > 0 ? a.bytes / target.rootBytes : 0)}</dd>
          </dl>
        )}
        <div className="tip__hint">Too small to draw individually. Zoom in or drill down.</div>
      </>
    );
  } else if (!info) {
    body = <div className="tip__name tip__loading">Loading…</div>;
  } else {
    const main = sizeMode === "allocated" ? info.allocated : info.logical;
    const t = describeTimestamp(info.modifiedMs);
    body = (
      <>
        <div className="tip__name">{info.name}</div>
        <dl className="tip__grid">
          <dt>On disk</dt>
          <dd>{fmt(info.allocated)}</dd>
          <dt>Logical</dt>
          <dd>{fmt(info.logical)}</dd>
          <dt>Share</dt>
          <dd>{formatPercent(target.rootBytes > 0 ? main / target.rootBytes : 0)} of this view</dd>
          {info.isDir && (
            <>
              <dt>Items</dt>
              <dd>{formatCount(info.items)}</dd>
            </>
          )}
          <dt>Category</dt>
          <dd>{categoryInfo(info.category).label}</dd>
          <dt>App</dt>
          <dd>{info.app ?? "Not attributed"}</dd>
          <dt>Modified</dt>
          <dd>
            {t.relative}
            {(t.suspicious || info.suspiciousTime) && <span className="badge badge--warn"> suspicious</span>}
          </dd>
          <dt>Safety</dt>
          <dd>{info.safety ? SAFETY_LABEL[info.safety] : "Not classified"}</dd>
        </dl>
      </>
    );
  }
  return (
    <div ref={ref} className="tip" role="tooltip">
      {body}
    </div>
  );
}
