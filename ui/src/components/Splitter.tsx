/**
 * Accessible pane splitter (`role="separator"`): drag with the pointer or
 * use the arrow keys (Shift for bigger steps), Home/End for the limits.
 */

/** Props for {@link Splitter}. */
export interface SplitterProps {
  /** Current size of the controlled pane in CSS px. */
  value: number;
  min: number;
  max: number;
  /** `horizontal` separates stacked panes (drag vertically). */
  orientation: "horizontal" | "vertical";
  label: string;
  /** Positive pointer travel grows the pane (`1`) or shrinks it (`-1`). */
  direction: 1 | -1;
  onChange: (value: number) => void;
}

/** A draggable, keyboard-operable divider. */
export function Splitter({ value, min, max, orientation, label, direction, onChange }: SplitterProps) {
  const clamp = (v: number) => Math.round(Math.min(max, Math.max(min, v)));
  return (
    <div
      role="separator"
      tabIndex={0}
      aria-label={label}
      aria-orientation={orientation}
      aria-valuenow={Math.round(value)}
      aria-valuemin={min}
      aria-valuemax={max}
      className={`splitter splitter--${orientation}`}
      onPointerDown={(e) => {
        e.preventDefault();
        const start = orientation === "horizontal" ? e.clientY : e.clientX;
        const startValue = value;
        const el = e.currentTarget;
        el.setPointerCapture(e.pointerId);
        const move = (ev: PointerEvent) => {
          const pos = orientation === "horizontal" ? ev.clientY : ev.clientX;
          onChange(clamp(startValue + (pos - start) * direction));
        };
        const up = () => {
          el.removeEventListener("pointermove", move);
          el.removeEventListener("pointerup", up);
        };
        el.addEventListener("pointermove", move);
        el.addEventListener("pointerup", up);
      }}
      onKeyDown={(e) => {
        const step = e.shiftKey ? 64 : 16;
        const grow = orientation === "horizontal" ? (e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0) : e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
        if (grow !== 0) onChange(clamp(value + grow * step * direction));
        else if (e.key === "Home") onChange(min);
        else if (e.key === "End") onChange(max);
        else return;
        e.preventDefault();
      }}
    />
  );
}
