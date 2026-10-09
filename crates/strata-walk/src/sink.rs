use strata_core::ScanRecord;

use crate::Progress;

/// Receives the output of a walk.
///
/// All calls happen on the thread that called [`crate::Walker::run`], so a
/// sink needs neither `Send` nor `Sync` and may hold UI handles or `&mut`
/// state. A slow sink applies backpressure: workers pause once a bounded
/// queue of pending batches fills.
///
/// Contract for `records`:
/// - Every [`ScanRecord::id`] is delivered exactly once.
/// - Hardlinked files arrive once with all their links (see the crate docs
///   for when that is possible).
/// - A directory may arrive before or after its children; consumers link by
///   id, not by order.
pub trait WalkSink {
    /// A batch of finished records.
    fn records(&mut self, batch: Vec<ScanRecord>);

    /// Periodic progress, at most once per
    /// [`crate::WalkOptions::progress_interval`], plus once at the end.
    fn progress(&mut self, _progress: &Progress) {}
}

/// Collects every record; convenient for tests and small trees.
impl WalkSink for Vec<ScanRecord> {
    fn records(&mut self, mut batch: Vec<ScanRecord>) {
        self.append(&mut batch);
    }
}

/// Adapts a pair of closures into a [`WalkSink`].
///
/// # Example
///
/// ```
/// use strata_walk::FnSink;
/// let mut count = 0usize;
/// let mut sink = FnSink::new(|batch: Vec<strata_core::ScanRecord>| count += batch.len());
/// # let _ = &mut sink;
/// ```
pub struct FnSink<R, P = fn(&Progress)> {
    on_records: R,
    on_progress: Option<P>,
}

impl<R> FnSink<R>
where
    R: FnMut(Vec<ScanRecord>),
{
    /// Sink that only receives records.
    pub fn new(on_records: R) -> Self {
        Self {
            on_records,
            on_progress: None,
        }
    }
}

impl<R, P> FnSink<R, P>
where
    R: FnMut(Vec<ScanRecord>),
    P: FnMut(&Progress),
{
    /// Sink that receives records and progress.
    pub fn with_progress(on_records: R, on_progress: P) -> Self {
        Self {
            on_records,
            on_progress: Some(on_progress),
        }
    }
}

impl<R, P> std::fmt::Debug for FnSink<R, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FnSink").finish_non_exhaustive()
    }
}

impl<R, P> WalkSink for FnSink<R, P>
where
    R: FnMut(Vec<ScanRecord>),
    P: FnMut(&Progress),
{
    fn records(&mut self, batch: Vec<ScanRecord>) {
        (self.on_records)(batch);
    }

    fn progress(&mut self, progress: &Progress) {
        if let Some(p) = &mut self.on_progress {
            p(progress);
        }
    }
}

/// A message produced by [`ChannelSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkEvent {
    /// A batch of records.
    Records(Vec<ScanRecord>),
    /// A progress update.
    Progress(Progress),
}

/// Forwards everything into a channel, for backends that consume the walk
/// on another thread (e.g. an IPC pump to the UI). Sends block when the
/// channel is full, which throttles the walk.
#[derive(Debug, Clone)]
pub struct ChannelSink {
    tx: crossbeam_channel::Sender<WalkEvent>,
}

impl ChannelSink {
    /// Wraps a sender.
    #[must_use]
    pub const fn new(tx: crossbeam_channel::Sender<WalkEvent>) -> Self {
        Self { tx }
    }
}

impl WalkSink for ChannelSink {
    fn records(&mut self, batch: Vec<ScanRecord>) {
        // NOTE: a dropped receiver means nobody wants the results; the walk
        // still runs to completion (cancel it via the token to stop early).
        let _ = self.tx.send(WalkEvent::Records(batch));
    }

    fn progress(&mut self, progress: &Progress) {
        let _ = self.tx.send(WalkEvent::Progress(*progress));
    }
}
