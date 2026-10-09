//! The layout pipe: index → `strata-layout` → binary frames on a Channel.
//!
//! Responsibilities:
//! - [`IndexSource`]: [`LayoutSource`] over a [`VolumeData`] (sizes in the
//!   requested [`SizeMode`], filtered children, the packed `color_key`).
//! - [`LayoutRequest`]: the UI's JSON request (`LayoutRequest` in
//!   `ui/src/lib/layout/stream.ts`).
//! - [`compute_frame`]: runs the view's layout and encodes one `STLF` frame,
//!   with transition records from the stream's previous rect layout.
//! - [`LayoutStream`]: per-canvas state: the frame sink, the newest
//!   requested sequence number (older pending requests are dropped) and the
//!   previous layout.
//!
//! Ids in the frames are wire ids ([`crate::ids`]); the source hands them to
//! the layout engine directly, so records carry them unchanged.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use strata_core::SizeMode;
use strata_index::EntryId;
use strata_layout::{
    CushionParams, IcicleConfig, IcicleOrientation, LayoutSource, MindMapConfig, NodeId,
    PackConfig, RectLayout, SunburstConfig, TreemapConfig, ViewTransform, layout_icicle,
    layout_mindmap, layout_pack, layout_sunburst, layout_treemap, transition_rects,
};

use crate::error::{CmdResult, CommandError};
use crate::frame::{FrameMeta, ViewKind, write_frame};
use crate::ids;
use crate::model::{ResolvedFilters, ViewFilters, VolumeData, now_epoch2000};

/// [`LayoutSource`] over one indexed volume.
#[derive(Debug)]
pub struct IndexSource<'a> {
    data: &'a VolumeData,
    mode: SizeMode,
    filters: ResolvedFilters,
    now: u32,
}

impl<'a> IndexSource<'a> {
    /// A source in `mode` applying `filters`.
    #[must_use]
    pub fn new(data: &'a VolumeData, mode: SizeMode, filters: &ViewFilters) -> Self {
        Self {
            data,
            mode,
            filters: data.resolve_filters(filters, mode),
            now: now_epoch2000(),
        }
    }

    /// Overrides "now" for age buckets (tests).
    #[must_use]
    pub const fn with_now(mut self, now: u32) -> Self {
        self.now = now;
        self
    }

    fn local(id: NodeId) -> EntryId {
        EntryId(ids::decode(id).1)
    }
}

impl LayoutSource for IndexSource<'_> {
    fn size(&self, id: NodeId) -> u64 {
        self.data.index.size(Self::local(id), self.mode)
    }

    fn children(&self, id: NodeId, out: &mut Vec<(NodeId, u64)>) {
        let ix = &self.data.index;
        for k in ix.children(Self::local(id)) {
            if self.filters.keep(self.data, k) {
                out.push((self.data.wire(k), ix.size(k, self.mode)));
            }
        }
    }

    fn is_dir(&self, id: NodeId) -> bool {
        self.data.index.is_dir(Self::local(id))
    }

    fn color_key(&self, id: NodeId) -> u32 {
        self.data.color_key(Self::local(id), self.now)
    }
}

/// Visual-zoom transform (`screen = layout * scale + t`).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct TransformDto {
    /// Zoom factor.
    pub scale: f64,
    /// X offset in device pixels.
    pub tx: f64,
    /// Y offset in device pixels.
    pub ty: f64,
}

/// Visual views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    /// Treemap.
    Treemap,
    /// Sunburst.
    Sunburst,
    /// Icicle.
    Icicle,
    /// Flame graph.
    Flame,
    /// Circle packing.
    Bubbles,
    /// Mind map.
    Mindmap,
}

/// Treemap style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Flat blocks.
    Flat,
    /// Cushion shading (adds the cushion section).
    Cushion,
}

/// One layout request.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutRequest {
    /// Volume id.
    pub volume_id: String,
    /// Root wire id.
    pub root: u32,
    /// View.
    pub view: View,
    /// Canvas width in device pixels.
    pub width: f32,
    /// Canvas height in device pixels.
    pub height: f32,
    /// Device pixel ratio.
    pub dpr: f32,
    /// Visual zoom.
    pub transform: TransformDto,
    /// Size mode.
    pub size_mode: SizeMode,
    /// Treemap style.
    pub style: Style,
    /// Filters.
    #[serde(default)]
    pub filters: ViewFilters,
    /// Emit transitions from the previous frame.
    #[serde(default)]
    pub animate: bool,
}

/// The rect layout a stream keeps for transitions.
#[derive(Debug)]
pub struct PrevLayout {
    view: ViewKind,
    generation: u8,
    rects: RectLayout,
}

/// Largest canvas side accepted (device pixels).
const MAX_SIDE: f32 = 32_768.0;

/// Lays out `req` and encodes the frame for `seq`. Returns the frame and,
/// for rect views, the layout to keep for the next transition.
///
/// # Errors
///
/// Bad viewport or an unknown root id.
pub fn compute_frame(
    data: &VolumeData,
    req: &LayoutRequest,
    seq: u32,
    prev: Option<&PrevLayout>,
) -> CmdResult<(Vec<u8>, Option<PrevLayout>)> {
    let ok = |v: f32| v.is_finite() && v > 0.0 && v <= MAX_SIDE;
    if !(ok(req.width) && ok(req.height) && req.dpr.is_finite() && req.dpr > 0.0) {
        return Err(CommandError::bad_request("invalid viewport"));
    }
    let root = data
        .resolve(req.root)
        .ok_or_else(|| CommandError::not_found("that folder is no longer in the index"))?;
    let root_wire = data.wire(root);
    let src = IndexSource::new(data, req.size_mode, &req.filters);
    let (w, h, dpr) = (req.width, req.height, req.dpr);
    let mut meta = FrameMeta {
        view: ViewKind::Treemap,
        seq,
        root: root_wire,
        root_bytes: src.size(root_wire),
        width: w,
        height: h,
        dpr,
        transform: ViewTransform::IDENTITY,
        center: (0.0, 0.0),
        ring_width: 0.0,
    };
    let rect_frame = |l: &RectLayout, meta: &FrameMeta, prev: Option<&PrevLayout>| {
        let transitions = match prev {
            Some(p) if req.animate && p.view == meta.view && p.generation == data.generation => {
                transition_rects(&p.rects, l).into_bytes()
            }
            _ => Vec::new(),
        };
        write_frame(
            meta,
            [
                l.rects().as_bytes(),
                l.aggregates().as_bytes(),
                l.labels().as_bytes(),
                l.cushions().map_or(&[][..], |c| c.as_bytes()),
                &transitions,
            ],
        )
    };
    let keep = |view, rects| {
        Some(PrevLayout {
            view,
            generation: data.generation,
            rects,
        })
    };
    Ok(match req.view {
        View::Treemap => {
            let t = req.transform;
            let transform = if t.scale.is_finite() && t.scale > 0.0 {
                ViewTransform {
                    scale: t.scale,
                    tx: t.tx,
                    ty: t.ty,
                }
            } else {
                ViewTransform::IDENTITY
            };
            let mut cfg = TreemapConfig::new(w, h, dpr).with_view(transform);
            if req.style == Style::Cushion {
                cfg.cushion = Some(CushionParams::default());
            }
            meta.transform = transform;
            let l = layout_treemap(&src, root_wire, &cfg);
            (rect_frame(&l, &meta, prev), keep(ViewKind::Treemap, l))
        }
        View::Icicle | View::Flame => {
            let mut cfg = IcicleConfig::new(w, h, dpr);
            let kind = if req.view == View::Flame {
                cfg.orientation = IcicleOrientation::BottomUp;
                ViewKind::Flame
            } else {
                ViewKind::Icicle
            };
            meta.view = kind;
            let l = layout_icicle(&src, root_wire, &cfg);
            (rect_frame(&l, &meta, prev), keep(kind, l))
        }
        View::Sunburst => {
            meta.view = ViewKind::Sunburst;
            let l = layout_sunburst(&src, root_wire, &SunburstConfig::new(w, h, dpr));
            meta.center = l.center();
            meta.ring_width = l.ring_width();
            let frame = write_frame(
                &meta,
                [
                    l.arcs().as_bytes(),
                    l.aggregates().as_bytes(),
                    &[],
                    &[],
                    &[],
                ],
            );
            (frame, None)
        }
        View::Bubbles | View::Mindmap => {
            let l = if req.view == View::Bubbles {
                meta.view = ViewKind::Bubbles;
                layout_pack(&src, root_wire, &PackConfig::new(w, h, dpr))
            } else {
                meta.view = ViewKind::MindMap;
                layout_mindmap(&src, root_wire, &MindMapConfig::new(w, h, dpr))
            };
            let frame = write_frame(
                &meta,
                [
                    l.circles().as_bytes(),
                    l.aggregates().as_bytes(),
                    l.labels().as_bytes(),
                    &[],
                    &[],
                ],
            );
            (frame, None)
        }
    })
}

/// Delivers an encoded frame to the UI (a Tauri Channel in the app, a
/// collector in tests).
pub type FrameSink = Box<dyn Fn(Vec<u8>) -> Result<(), String> + Send + Sync>;

/// Request → frame-sent timings of one stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutTiming {
    /// Frames sent.
    pub frames: u64,
    /// Requests dropped because a newer one was queued.
    pub dropped: u64,
    /// Duration of the last frame (layout + encode + send).
    pub last: Duration,
    /// Slowest frame.
    pub max: Duration,
}

/// One canvas's layout stream.
pub struct LayoutStream {
    sink: FrameSink,
    latest: AtomicU32,
    prev: Mutex<Option<PrevLayout>>,
    timing: Mutex<LayoutTiming>,
}

impl std::fmt::Debug for LayoutStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutStream")
            .field("latest", &self.latest)
            .finish_non_exhaustive()
    }
}

impl LayoutStream {
    /// A stream sending frames to `sink`.
    #[must_use]
    pub fn new(sink: FrameSink) -> Self {
        Self {
            sink,
            latest: AtomicU32::new(0),
            prev: Mutex::new(None),
            timing: Mutex::new(LayoutTiming::default()),
        }
    }

    /// Records that `seq` was requested; newer requests make older pending
    /// ones stale.
    pub fn announce(&self, seq: u32) {
        self.latest.fetch_max(seq, Ordering::AcqRel);
    }

    /// Whether a newer request than `seq` is queued.
    #[must_use]
    pub fn is_stale(&self, seq: u32) -> bool {
        self.latest.load(Ordering::Acquire) > seq
    }

    /// Lays out and sends the frame for `seq`, unless a newer request is
    /// already queued (then nothing is sent and `Ok(false)` is returned).
    ///
    /// # Errors
    ///
    /// Layout or delivery failures.
    pub fn serve(&self, data: &VolumeData, req: &LayoutRequest, seq: u32) -> CmdResult<bool> {
        let started = Instant::now();
        if self.is_stale(seq) {
            lock(&self.timing).dropped += 1;
            return Ok(false);
        }
        let mut prev = lock(&self.prev);
        let (frame, keep) = compute_frame(data, req, seq, prev.as_ref())?;
        if keep.is_some() || req.view != View::Treemap {
            *prev = keep;
        }
        drop(prev);
        (self.sink)(frame).map_err(CommandError::internal)?;
        let mut t = lock(&self.timing);
        t.frames += 1;
        t.last = started.elapsed();
        t.max = t.max.max(t.last);
        Ok(true)
    }

    /// Timings so far.
    #[must_use]
    pub fn timing(&self) -> LayoutTiming {
        *lock(&self.timing)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
