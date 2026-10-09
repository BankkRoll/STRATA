/**
 * Decoding of layout frames: the binary container the backend sends over the
 * layout Channel, holding `strata-layout` buffers byte for byte.
 *
 * Responsibilities:
 * - Validate the 128-byte frame header and its section table.
 * - Expose each section as zero-copy typed-array views ({@link NodeBuffer},
 *   {@link AggregateTable}, {@link LabelTable}, cushions, {@link TransitionBuffer}).
 *
 * Byte layouts are specified in `strata-layout`'s `buffer` module (records)
 * and `src-tauri/src/frame.rs` (frame container). Everything is
 * little-endian, which matches every platform WebView2 runs on; typed arrays
 * are used directly.
 */

/** `"STLF"` read as a little-endian u32. */
export const FRAME_MAGIC = 0x464c5453;
/** Frame container version this build understands. */
export const FRAME_VERSION = 1;
/** Size of the frame header in bytes. */
export const FRAME_HEADER_BYTES = 128;

/** Bytes per node record (rect, arc and circle records alike). */
export const NODE_STRIDE = 32;
/** Bytes per aggregate side-table record. */
export const AGGREGATE_STRIDE = 24;
/** Bytes per label record. */
export const LABEL_STRIDE = 32;
/** Bytes per cushion record. */
export const CUSHION_STRIDE = 16;
/** Bytes per transition record. */
export const TRANSITION_STRIDE = 48;

/** Record index meaning "none" (the root's parent, an undrawn aggregate). */
export const NO_INDEX = 0xffffffff;

/** Per-record flag bits (`u16` at offset 30 of every node record). */
export const NodeFlag = {
  /** Directory. */
  DIR: 1 << 0,
  /** Treemap: top strip reserved for name + size. */
  HAS_HEADER: 1 << 1,
  /** "N small items" block; `id` is the parent directory's id. Hatched. */
  AGGREGATE: 1 << 2,
  /** A real entry the user can select (everything but aggregates). */
  SELECTABLE: 1 << 3,
  /** Directory whose children were not laid out. */
  TRUNCATED: 1 << 4,
  /** Treemap: geometry was clipped to the viewport. */
  CLIPPED: 1 << 5,
} as const;

/** Which layout produced a frame (`u16` at header offset 6). */
export const ViewKind = {
  Treemap: 0,
  Icicle: 1,
  Flame: 2,
  Sunburst: 3,
  Bubbles: 4,
  MindMap: 5,
} as const;

/** Numeric view kind. */
export type ViewKind = (typeof ViewKind)[keyof typeof ViewKind];

/** Transition kind (`u16` at offset 44 of a transition record). */
export const TransitionKind = {
  Stay: 0,
  Appear: 1,
  Disappear: 2,
} as const;

/** Visual-zoom transform: `screen = layout * scale + (tx, ty)`. */
export interface ViewTransform {
  scale: number;
  tx: number;
  ty: number;
}

/** The identity transform. */
export const IDENTITY_TRANSFORM: Readonly<ViewTransform> = Object.freeze({ scale: 1, tx: 0, ty: 0 });

/** Raised for malformed frames; the renderer keeps the previous frame. */
export class FrameError extends Error {
  override name = "FrameError";
}

/**
 * Zero-copy accessor over a node buffer (rect, arc or circle records).
 *
 * Geometry lives in `f32[i*8 .. i*8+4]`; the tail is `u32[i*8+4]` (id),
 * `u32[i*8+5]` (color key), `u32[i*8+6]` (parent), `u16[i*16+14]` (depth)
 * and `u16[i*16+15]` (flags).
 */
export class NodeBuffer {
  /** The section bytes, exactly as uploaded to the GPU. */
  readonly bytes: Uint8Array;
  /** Float view (8 floats per record). */
  readonly f32: Float32Array;
  /** u32 view (8 words per record). */
  readonly u32: Uint32Array;
  /** u16 view (16 halves per record). */
  readonly u16: Uint16Array;
  /** Number of records. */
  readonly count: number;

  /**
   * @param bytes - Section bytes; `byteOffset` must be 4-byte aligned and the
   *   length a multiple of {@link NODE_STRIDE}.
   */
  constructor(bytes: Uint8Array) {
    if (bytes.byteLength % NODE_STRIDE !== 0) {
      throw new FrameError(`node section length ${bytes.byteLength} is not a multiple of ${NODE_STRIDE}`);
    }
    this.bytes = bytes;
    this.count = bytes.byteLength / NODE_STRIDE;
    this.f32 = new Float32Array(bytes.buffer, bytes.byteOffset, this.count * 8);
    this.u32 = new Uint32Array(bytes.buffer, bytes.byteOffset, this.count * 8);
    this.u16 = new Uint16Array(bytes.buffer, bytes.byteOffset, this.count * 16);
  }

  /** Geometry component `k` (0–3) of record `i`. */
  geom(i: number, k: number): number {
    return this.f32[i * 8 + k] ?? 0;
  }
  /** Entry id of record `i` (the directory id for aggregates). */
  id(i: number): number {
    return this.u32[i * 8 + 4] ?? 0;
  }
  /** Packed color key of record `i`. */
  colorKey(i: number): number {
    return this.u32[i * 8 + 5] ?? 0;
  }
  /** Parent record index, or {@link NO_INDEX}. */
  parent(i: number): number {
    return this.u32[i * 8 + 6] ?? NO_INDEX;
  }
  /** Depth below the layout root. */
  depth(i: number): number {
    return this.u16[i * 16 + 14] ?? 0;
  }
  /** Flag bits ({@link NodeFlag}). */
  flags(i: number): number {
    return this.u16[i * 16 + 15] ?? 0;
  }
  /** Whether record `i` is a selectable real entry. */
  selectable(i: number): boolean {
    return (this.flags(i) & NodeFlag.SELECTABLE) !== 0;
  }
}

/** Reads a little-endian u64 as a JS number (exact below 2^53, i.e. 8 PiB). */
function u64(view: DataView, off: number): number {
  return view.getUint32(off, true) + view.getUint32(off + 4, true) * 0x1_0000_0000;
}

/** One "N small items" side-table entry. */
export interface AggregateEntry {
  /** Index of the hatched record, or {@link NO_INDEX}. */
  record: number;
  /** Index of the directory's record. */
  parent: number;
  /** Directory id. */
  dirId: number;
  /** Number of children folded. */
  count: number;
  /** Their total bytes. */
  bytes: number;
}

/** Accessor over the aggregate side table (24-byte records). */
export class AggregateTable {
  /** Number of entries. */
  readonly count: number;
  private readonly view: DataView;
  private byRecord: Map<number, number> | null = null;

  /** @param bytes - Section bytes. */
  constructor(bytes: Uint8Array) {
    if (bytes.byteLength % AGGREGATE_STRIDE !== 0) {
      throw new FrameError(`aggregate section length ${bytes.byteLength} is not a multiple of ${AGGREGATE_STRIDE}`);
    }
    this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    this.count = bytes.byteLength / AGGREGATE_STRIDE;
  }

  /** Decodes entry `i`. */
  get(i: number): AggregateEntry {
    const o = i * AGGREGATE_STRIDE;
    return {
      record: this.view.getUint32(o, true),
      parent: this.view.getUint32(o + 4, true),
      dirId: this.view.getUint32(o + 8, true),
      count: this.view.getUint32(o + 12, true),
      bytes: u64(this.view, o + 16),
    };
  }

  /**
   * Finds the entry whose hatched block is node record `record`.
   *
   * @param record - Node record index of an `AGGREGATE` record.
   * @returns The entry, or `null`.
   */
  forRecord(record: number): AggregateEntry | null {
    if (!this.byRecord) {
      this.byRecord = new Map();
      for (let i = 0; i < this.count; i++) {
        this.byRecord.set(this.view.getUint32(i * AGGREGATE_STRIDE, true), i);
      }
    }
    const i = this.byRecord.get(record);
    return i === undefined ? null : this.get(i);
  }
}

/** One label candidate. */
export interface LabelEntry {
  /** Labelled node record. */
  record: number;
  /** Entry id (directory id for aggregates). */
  id: number;
  x: number;
  y: number;
  w: number;
  h: number;
  /** Bytes to print with the name. */
  size: number;
}

/** Accessor over label candidates (32-byte records). */
export class LabelTable {
  /** Number of labels. */
  readonly count: number;
  private readonly view: DataView;

  /** @param bytes - Section bytes. */
  constructor(bytes: Uint8Array) {
    if (bytes.byteLength % LABEL_STRIDE !== 0) {
      throw new FrameError(`label section length ${bytes.byteLength} is not a multiple of ${LABEL_STRIDE}`);
    }
    this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    this.count = bytes.byteLength / LABEL_STRIDE;
  }

  /** Decodes label `i`. */
  get(i: number): LabelEntry {
    const o = i * LABEL_STRIDE;
    const v = this.view;
    return {
      record: v.getUint32(o, true),
      id: v.getUint32(o + 4, true),
      x: v.getFloat32(o + 8, true),
      y: v.getFloat32(o + 12, true),
      w: v.getFloat32(o + 16, true),
      h: v.getFloat32(o + 20, true),
      size: u64(v, o + 24),
    };
  }
}

/**
 * Accessor over transition records (48 bytes): `from` vec4 at 0, `to` vec4 at
 * 16, then `id, old_index, new_index` u32 and `kind, flags` u16.
 */
export class TransitionBuffer {
  /** The section bytes, uploaded to the GPU as-is. */
  readonly bytes: Uint8Array;
  /** Float view (12 floats per record). */
  readonly f32: Float32Array;
  /** u32 view (12 words per record). */
  readonly u32: Uint32Array;
  /** Number of records. */
  readonly count: number;

  /** @param bytes - Section bytes, 4-byte aligned. */
  constructor(bytes: Uint8Array) {
    if (bytes.byteLength % TRANSITION_STRIDE !== 0) {
      throw new FrameError(`transition section length ${bytes.byteLength} is not a multiple of ${TRANSITION_STRIDE}`);
    }
    this.bytes = bytes;
    this.count = bytes.byteLength / TRANSITION_STRIDE;
    this.f32 = new Float32Array(bytes.buffer, bytes.byteOffset, this.count * 12);
    this.u32 = new Uint32Array(bytes.buffer, bytes.byteOffset, this.count * 12);
  }

  /** Entry id of transition `i`. */
  id(i: number): number {
    return this.u32[i * 12 + 8] ?? 0;
  }
  /** Old record index, or {@link NO_INDEX}. */
  oldIndex(i: number): number {
    return this.u32[i * 12 + 9] ?? NO_INDEX;
  }
  /** New record index, or {@link NO_INDEX}. */
  newIndex(i: number): number {
    return this.u32[i * 12 + 10] ?? NO_INDEX;
  }
  /** Kind ({@link TransitionKind}). */
  kind(i: number): number {
    return (this.u32[i * 12 + 11] ?? 0) & 0xffff;
  }
  /** Flags of the record being shown. */
  flags(i: number): number {
    return (this.u32[i * 12 + 11] ?? 0) >>> 16;
  }
}

/** A decoded layout frame. */
export interface LayoutFrame {
  /** Producing layout. */
  view: ViewKind;
  /** Request sequence number this frame answers. */
  seq: number;
  /** Entry id of the layout root. */
  root: number;
  /** Root size in the active size mode (for percentages). */
  rootBytes: number;
  /** Canvas size in device pixels the layout was computed for. */
  width: number;
  height: number;
  /** Device pixel ratio the layout was computed for. */
  dpr: number;
  /** Visual-zoom transform the layout was computed under. */
  transform: ViewTransform;
  /** Sunburst center (device px). */
  centerX: number;
  centerY: number;
  /** Sunburst ring width (device px). */
  ringWidth: number;
  /** Main node buffer. */
  nodes: NodeBuffer;
  /** "N small items" side table. */
  aggregates: AggregateTable;
  /** Label candidates. */
  labels: LabelTable;
  /** Cushion coefficients parallel to `nodes` (4 floats per record), or `null`. */
  cushions: Float32Array | null;
  /** Transitions from the previous frame, or `null`. */
  transitions: TransitionBuffer | null;
}

/**
 * Decodes a layout frame without copying the section bytes.
 *
 * @param input - The frame as received from the Channel (or a fixture file).
 *   A `Uint8Array` whose offset is not 4-byte aligned is copied once.
 * @returns The decoded frame.
 * @throws {FrameError} On bad magic, version, view kind or section bounds.
 */
export function decodeFrame(input: ArrayBuffer | Uint8Array): LayoutFrame {
  let bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  if (bytes.byteOffset % 4 !== 0) bytes = bytes.slice();
  if (bytes.byteLength < FRAME_HEADER_BYTES) {
    throw new FrameError(`frame is ${bytes.byteLength} bytes; header needs ${FRAME_HEADER_BYTES}`);
  }
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (v.getUint32(0, true) !== FRAME_MAGIC) throw new FrameError("bad frame magic");
  const version = v.getUint16(4, true);
  if (version !== FRAME_VERSION) throw new FrameError(`unsupported frame version ${version}`);
  const view = v.getUint16(6, true);
  if (view > ViewKind.MindMap) throw new FrameError(`unknown view kind ${view}`);

  const section = (k: number, align: number): Uint8Array => {
    const off = v.getUint32(72 + k * 8, true);
    const len = v.getUint32(76 + k * 8, true);
    if (len === 0) return new Uint8Array(bytes.buffer, bytes.byteOffset, 0);
    if (off < FRAME_HEADER_BYTES || off + len > bytes.byteLength || off % align !== 0) {
      throw new FrameError(`section ${k} out of bounds (offset ${off}, length ${len})`);
    }
    return new Uint8Array(bytes.buffer, bytes.byteOffset + off, len);
  };

  const cushionBytes = section(3, 4);
  if (cushionBytes.byteLength % CUSHION_STRIDE !== 0) {
    throw new FrameError("cushion section length is not a multiple of 16");
  }
  const nodes = new NodeBuffer(section(0, 4));
  if (cushionBytes.byteLength !== 0 && cushionBytes.byteLength / CUSHION_STRIDE !== nodes.count) {
    throw new FrameError("cushion section is not parallel to the node section");
  }
  const transitionBytes = section(4, 4);

  return {
    view: view as ViewKind,
    seq: v.getUint32(8, true),
    root: v.getUint32(12, true),
    width: v.getFloat32(16, true),
    height: v.getFloat32(20, true),
    dpr: v.getFloat32(24, true),
    transform: {
      scale: v.getFloat64(32, true),
      tx: v.getFloat64(40, true),
      ty: v.getFloat64(48, true),
    },
    centerX: v.getFloat32(56, true),
    centerY: v.getFloat32(60, true),
    ringWidth: v.getFloat32(64, true),
    rootBytes: u64(v, 112),
    nodes,
    aggregates: new AggregateTable(section(1, 4)),
    labels: new LabelTable(section(2, 4)),
    cushions:
      cushionBytes.byteLength === 0
        ? null
        : new Float32Array(cushionBytes.buffer, cushionBytes.byteOffset, cushionBytes.byteLength / 4),
    transitions: transitionBytes.byteLength === 0 ? null : new TransitionBuffer(transitionBytes),
  };
}
