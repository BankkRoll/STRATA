/**
 * The layout pipe between the backend and the renderers.
 *
 * The UI sends a small JSON {@link LayoutRequest} (`layout_request`); the
 * backend lays out with `strata-layout` and answers with one binary layout
 * frame per request on a Tauri Channel opened by `layout_open`. Frames are
 * decoded zero-copy ({@link decodeFrame}).
 *
 * Responsibilities:
 * - {@link LayoutStream}: the interface renderers and views depend on.
 * - {@link TauriLayoutStream}: the real implementation; coalesces requests
 *   (at most one in flight, latest pending wins) so resize and wheel bursts
 *   never queue stale layouts, and drops out-of-order frames.
 */
import { Channel } from "@tauri-apps/api/core";
import { call } from "../backend";
import type { SizeMode, TreemapStyle, ViewFilters, VisualView } from "../types";
import { decodeFrame, type LayoutFrame, type ViewTransform } from "./frame";

/** Parameters of one layout (JSON, camelCase, sent with `layout_request`). */
export interface LayoutRequest {
  /** Volume the root belongs to. */
  volumeId: string;
  /** Entry id of the layout root. */
  root: number;
  /** Which layout to run. */
  view: VisualView;
  /** Canvas size in device pixels. */
  width: number;
  height: number;
  /** `devicePixelRatio` of the monitor the canvas is on. */
  dpr: number;
  /** Visual-zoom transform (treemap only; identity otherwise). */
  transform: ViewTransform;
  /** Size mode for areas and labels. */
  sizeMode: SizeMode;
  /** Treemap style: `cushion` asks for the cushion section. */
  style: TreemapStyle;
  /** Active filters. */
  filters: ViewFilters;
  /**
   * Include a transitions section from this stream's previous frame (drill,
   * go up, live changes). Treemap/icicle only; other views tween in JS.
   */
  animate: boolean;
}

/** Receives decoded frames. */
export type FrameListener = (frame: LayoutFrame) => void;
/** Receives layout failures (bad frame, backend error). */
export type LayoutErrorListener = (error: unknown) => void;

/** A source of layout frames for one canvas. */
export interface LayoutStream {
  /**
   * Asks for a layout. Returns the sequence number the answering frame will
   * carry. Superseded requests may never be answered.
   */
  request(req: LayoutRequest): number;
  /** Subscribes to frames; returns an unsubscribe function. */
  subscribe(listener: FrameListener): () => void;
  /** Subscribes to errors; returns an unsubscribe function. */
  onError(listener: LayoutErrorListener): () => void;
  /** Releases the backend stream. */
  close(): void;
}

/** Base class handling listeners and stale-frame dropping. */
export abstract class BaseLayoutStream implements LayoutStream {
  private readonly frameListeners = new Set<FrameListener>();
  private readonly errorListeners = new Set<LayoutErrorListener>();
  private lastDelivered = 0;

  abstract request(req: LayoutRequest): number;
  abstract close(): void;

  subscribe(listener: FrameListener): () => void {
    this.frameListeners.add(listener);
    return () => this.frameListeners.delete(listener);
  }

  onError(listener: LayoutErrorListener): () => void {
    this.errorListeners.add(listener);
    return () => this.errorListeners.delete(listener);
  }

  /** Decodes and delivers a frame unless an equal or newer one was delivered. */
  protected deliver(bytes: ArrayBuffer | Uint8Array): LayoutFrame | null {
    let frame: LayoutFrame;
    try {
      frame = decodeFrame(bytes);
    } catch (err) {
      this.fail(err);
      return null;
    }
    if (frame.seq <= this.lastDelivered) return null;
    this.lastDelivered = frame.seq;
    for (const l of this.frameListeners) l(frame);
    return frame;
  }

  /** Reports an error to listeners. */
  protected fail(err: unknown): void {
    for (const l of this.errorListeners) l(err);
  }
}

/**
 * Layout stream backed by the Tauri commands `layout_open`,
 * `layout_request` and `layout_close`.
 */
export class TauriLayoutStream extends BaseLayoutStream {
  private readonly channel = new Channel<ArrayBuffer>();
  private readonly streamId: Promise<number>;
  private seq = 0;
  private inFlight: number | null = null;
  private pending: { seq: number; req: LayoutRequest } | null = null;
  private closed = false;

  constructor() {
    super();
    this.channel.onmessage = (msg) => {
      this.deliver(msg);
    };
    this.streamId = call<{ streamId: number }>("layout_open", { onFrame: this.channel }).then((r) => r.streamId);
    this.streamId.catch((err: unknown) => {
      this.fail(err);
    });
  }

  request(req: LayoutRequest): number {
    const seq = ++this.seq;
    this.pending = { seq, req };
    this.flush();
    return seq;
  }

  private flush(): void {
    if (this.closed || this.inFlight !== null || !this.pending) return;
    const { seq, req } = this.pending;
    this.pending = null;
    this.inFlight = seq;
    // The command resolves once its frame has been sent on the channel, which
    // is what frees the slot for the next pending request.
    this.streamId
      .then((streamId) => call<null>("layout_request", { streamId, seq, request: req }))
      .catch((err: unknown) => {
        this.fail(err);
      })
      .finally(() => {
        this.inFlight = null;
        this.flush();
      });
  }

  close(): void {
    this.closed = true;
    this.streamId
      .then((streamId) => call<null>("layout_close", { streamId }))
      .catch(() => {
        // Closing a stream the backend never opened is not an error worth surfacing.
      });
  }
}
