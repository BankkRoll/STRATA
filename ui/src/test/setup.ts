/**
 * Vitest setup for jsdom tests: layout and graphics APIs jsdom lacks.
 * Elements report an 800×600 box so virtualized lists render rows, and a
 * ResizeObserver reports that size; canvases have no WebGL (the GL layer is
 * replaced by a fake renderer in tests).
 */
import { afterEach } from "vitest";

if (typeof window !== "undefined") {
  const { cleanup } = await import("@testing-library/react");
  afterEach(() => {
    cleanup();
  });

  const rect = (): DOMRect => ({ x: 0, y: 0, left: 0, top: 0, right: 800, bottom: 600, width: 800, height: 600, toJSON: () => ({}) });
  Element.prototype.getBoundingClientRect = rect;
  Object.defineProperty(HTMLElement.prototype, "offsetHeight", { configurable: true, get: () => 600 });
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, get: () => 800 });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", { configurable: true, get: () => 600 });

  Element.prototype.setPointerCapture = () => undefined;
  Element.prototype.releasePointerCapture = () => undefined;
  Element.prototype.hasPointerCapture = () => false;

  HTMLCanvasElement.prototype.getContext = () => null;

  class FakeResizeObserver {
    constructor(private readonly cb: ResizeObserverCallback) {}
    observe(target: Element): void {
      const size = [{ inlineSize: 800, blockSize: 600 }];
      const entry: ResizeObserverEntry = {
        target,
        contentRect: rect(),
        borderBoxSize: size,
        contentBoxSize: size,
        devicePixelContentBoxSize: size,
      };
      queueMicrotask(() => {
        this.cb([entry], this);
      });
    }
    unobserve(): void {
      // Nothing to stop.
    }
    disconnect(): void {
      // Nothing to stop.
    }
  }
  window.ResizeObserver = FakeResizeObserver;
}
