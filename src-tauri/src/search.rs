//! Streaming filename search for the command palette.
//!
//! A [`SearchStream`] belongs to one palette. Every [`SearchStream::query`]
//! cancels the previous query (its [`CancelToken`]) and runs the new one on
//! a background thread over each indexed volume, using `strata-index`'s
//! parallel name scan with the classifier's safety tiers (`safe:`) and the
//! app table (`app:`). Hits are merged across volumes, ranked, and sent as
//! [`SearchBatch`]es of at most [`BATCH`] results; the last batch has
//! `done: true` and the total match count. A cancelled query sends nothing
//! further, and the UI drops batches whose `seq` is not its latest.
//!
//! Ranked delivery (rather than streaming unsorted chunk results) is used
//! because a complete scan of 5M names takes ~25 ms, inside the 50 ms
//! first-results budget, and the palette appends batches in arrival order.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use strata_core::{Safety, SizeMode};
use strata_index::search::{CancelToken, Hit, Query, SearchOptions, SearchSort};

use crate::classify::Engine;
use crate::model::{VolumeData, now_filetime};
use crate::scan::DataSlot;

/// Results per batch.
pub const BATCH: usize = 256;
/// Hits ranked per query.
pub const LIMIT: usize = 1000;

/// `SearchQuery` in `ui/src/lib/search.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    /// Search box text (filters included).
    pub text: String,
    /// Treat name terms as regular expressions.
    #[serde(default)]
    pub regex: bool,
    /// Case-sensitive names.
    #[serde(default)]
    pub case_sensitive: bool,
    /// One volume, or all when `None`.
    pub volume_id: Option<String>,
}

/// One hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// Volume id.
    pub volume_id: String,
    /// Wire id.
    pub id: u32,
    /// Name.
    pub name: String,
    /// Parent folder path.
    pub parent_path: String,
    /// Directory.
    pub is_dir: bool,
    /// Allocated bytes.
    pub allocated: u64,
    /// Logical bytes.
    pub logical: u64,
}

/// One batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchBatch {
    /// Query sequence number.
    pub seq: u32,
    /// Results.
    pub results: Vec<SearchResult>,
    /// Last batch of this query.
    pub done: bool,
    /// Total matches, on the last batch.
    pub total: Option<u64>,
}

/// Delivers batches (a Tauri Channel in the app).
pub type BatchSink = Arc<dyn Fn(SearchBatch) + Send + Sync>;

/// A volume to search.
#[derive(Debug, Clone)]
pub struct Target {
    /// Volume id.
    pub volume_id: String,
    /// Its index slot.
    pub slot: Arc<DataSlot>,
}

/// One palette's search stream.
pub struct SearchStream {
    sink: BatchSink,
    current: Mutex<Option<CancelToken>>,
}

impl std::fmt::Debug for SearchStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchStream").finish_non_exhaustive()
    }
}

impl SearchStream {
    /// A stream delivering to `sink`.
    #[must_use]
    pub fn new(sink: BatchSink) -> Self {
        Self {
            sink,
            current: Mutex::new(None),
        }
    }

    /// Cancels the running query, if any.
    pub fn cancel(&self) {
        if let Some(t) = lock(&self.current).take() {
            t.cancel();
        }
    }

    /// Starts query `seq`, cancelling the previous one. Returns the handle
    /// of the worker thread.
    pub fn query(
        &self,
        seq: u32,
        q: SearchQuery,
        targets: Vec<Target>,
        engine: Option<Arc<Engine>>,
    ) -> std::thread::JoinHandle<()> {
        let token = CancelToken::new();
        if let Some(old) = lock(&self.current).replace(token.clone()) {
            old.cancel();
        }
        let sink = self.sink.clone();
        std::thread::spawn(move || run_query(seq, &q, &targets, engine.as_deref(), &token, &*sink))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run_query(
    seq: u32,
    q: &SearchQuery,
    targets: &[Target],
    engine: Option<&Engine>,
    token: &CancelToken,
    sink: &(dyn Fn(SearchBatch) + Send + Sync),
) {
    let text = q.text.trim();
    let empty = SearchBatch {
        seq,
        results: Vec::new(),
        done: true,
        total: Some(0),
    };
    let parsed = if q.regex {
        Query::parse_regex(text, now_filetime())
    } else {
        Query::parse(text, now_filetime())
    };
    let mut parsed = match parsed {
        Ok(p) if !text.is_empty() => p,
        _ => {
            sink(empty);
            return;
        }
    };
    parsed.case_sensitive |= q.case_sensitive;
    let mut ranked: Vec<(Hit, SearchResult)> = Vec::new();
    let mut total = 0u64;
    for t in targets {
        if token.is_cancelled() {
            return;
        }
        if q.volume_id.as_ref().is_some_and(|v| v != &t.volume_id) {
            continue;
        }
        let guard = t
            .slot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(data) = guard.as_ref() else { continue };
        if let Some(letter) = parsed.volume
            && !data.root_path.to_ascii_uppercase().starts_with(letter)
        {
            continue;
        }
        let Ok((hits, matched)) = search_one(data, &parsed, engine, token) else {
            continue;
        };
        total += matched;
        ranked.extend(hits.into_iter().map(|h| (h, result(data, &t.volume_id, h))));
    }
    if token.is_cancelled() {
        return;
    }
    ranked.sort_by(|a, b| b.0.score.cmp(&a.0.score).then(b.0.size.cmp(&a.0.size)));
    ranked.truncate(LIMIT);
    let results: Vec<SearchResult> = ranked.into_iter().map(|(_, r)| r).collect();
    let mut chunks = results.chunks(BATCH).peekable();
    if chunks.peek().is_none() {
        sink(SearchBatch {
            seq,
            results: Vec::new(),
            done: true,
            total: Some(total),
        });
    }
    while let Some(c) = chunks.next() {
        if token.is_cancelled() {
            return;
        }
        let done = chunks.peek().is_none();
        sink(SearchBatch {
            seq,
            results: c.to_vec(),
            done,
            total: done.then_some(total),
        });
    }
}

fn search_one(
    data: &VolumeData,
    q: &Query,
    engine: Option<&Engine>,
    token: &CancelToken,
) -> Result<(Vec<Hit>, u64), ()> {
    let safety = |id: strata_index::EntryId| data.class(id).map_or(Safety::Careful, |c| c.safety);
    let app_matches = |id: u32, needle: &str| engine.is_some_and(|e| e.apps.matches(id, needle));
    let opts = SearchOptions {
        limit: LIMIT,
        sort: SearchSort::Relevance,
        mode: SizeMode::Allocated,
        safety: Some(&safety),
        app_matches: engine
            .is_some()
            .then_some(&app_matches as &(dyn Fn(u32, &str) -> bool + Sync)),
        ..SearchOptions::default()
    };
    let out = data
        .index
        .search(q, &opts, token, &|_| {})
        .map_err(|_| ())?;
    if out.cancelled {
        return Err(());
    }
    Ok((out.hits, out.matched))
}

fn result(data: &VolumeData, volume_id: &str, h: Hit) -> SearchResult {
    let ix = &data.index;
    SearchResult {
        volume_id: volume_id.to_owned(),
        id: data.wire(h.id),
        name: ix.name_lossy(h.id),
        parent_path: ix
            .parent(h.id)
            .map(|p| ix.path_string(p))
            .unwrap_or_default(),
        is_dir: ix.is_dir(h.id),
        allocated: ix.size(h.id, SizeMode::Allocated),
        logical: ix.size(h.id, SizeMode::Logical),
    }
}
