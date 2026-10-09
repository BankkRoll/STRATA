/**
 * One tooltip for the whole shell: any element with `data-tip` shows its
 * text after a short hover or on keyboard focus. Rendered in a fixed layer so
 * scroll containers never clip it, and placed beside the element, flipping
 * to stay inside the window.
 *
 * The text is supplementary: every control carrying `data-tip` also has an
 * accessible name, so the layer itself is hidden from assistive technology.
 */
import { useEffect, useRef, useState } from "react";

const DELAY_MS = 450;

interface Tip {
  text: string;
  rect: DOMRect;
}

/** The tooltip layer; mount once. */
export function TipLayer() {
  const [tip, setTip] = useState<Tip | null>(null);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    let current: HTMLElement | null = null;
    const hide = () => {
      if (timer !== null) clearTimeout(timer);
      timer = null;
      current = null;
      setTip(null);
    };
    const show = (el: HTMLElement, delay: number) => {
      if (el === current) return;
      hide();
      current = el;
      timer = setTimeout(() => {
        const text = el.dataset.tip;
        if (text && el.isConnected) setTip({ text, rect: el.getBoundingClientRect() });
      }, delay);
    };
    const over = (e: PointerEvent) => {
      const el = (e.target as Element | null)?.closest<HTMLElement>("[data-tip]");
      if (el) show(el, DELAY_MS);
      else if (current) hide();
    };
    const focus = (e: FocusEvent) => {
      const el = e.target instanceof HTMLElement ? e.target.closest<HTMLElement>("[data-tip]") : null;
      if (el?.matches(":focus-visible")) show(el, 0);
      else hide();
    };
    document.addEventListener("pointerover", over);
    document.addEventListener("focusin", focus);
    document.addEventListener("pointerdown", hide, true);
    document.addEventListener("keydown", hide, true);
    window.addEventListener("blur", hide);
    return () => {
      hide();
      document.removeEventListener("pointerover", over);
      document.removeEventListener("focusin", focus);
      document.removeEventListener("pointerdown", hide, true);
      document.removeEventListener("keydown", hide, true);
      window.removeEventListener("blur", hide);
    };
  }, []);

  useEffect(() => {
    const el = ref.current;
    if (!el || !tip) return;
    const r = tip.rect;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const gap = 8;
    let left: number;
    let top: number;
    // Narrow targets on the left edge (the activity bar) get the tip beside them.
    if (r.left < 64 && r.width <= 48) {
      left = r.right + gap;
      top = r.top + (r.height - h) / 2;
    } else {
      left = r.left + (r.width - w) / 2;
      top = r.bottom + gap + h > vh ? r.top - gap - h : r.bottom + gap;
    }
    el.style.left = `${Math.round(Math.max(8, Math.min(vw - w - 8, left)))}px`;
    el.style.top = `${Math.round(Math.max(8, Math.min(vh - h - 8, top)))}px`;
  }, [tip]);

  if (!tip) return null;
  return (
    <div ref={ref} className="tiplayer" aria-hidden="true">
      {tip.text}
    </div>
  );
}
