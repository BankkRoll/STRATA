//! Everything-style name search.
//!
//! [`Query::parse`] turns the search box text into name terms and filters
//! (syntax in [`parse`]); [`Index::search`] scans the name buffer in parallel
//! chunks, streams matches to a callback as each chunk finishes, honours a
//! [`CancelToken`] (a new keystroke cancels the previous query), and returns
//! the best `limit` hits sorted by relevance or size.
//!
//! The scan is a sequential pass over the WTF-8 name buffer per chunk (names
//! of base entries are stored in id order), with column filters checked
//! before any name work. No auxiliary index is needed to meet the latency
//! target; see `docs/BENCHMARKS.md` for numbers.
//!
//! # Example
//!
//! ```
//! use std::sync::Mutex;
//! use strata_core::*;
//! use strata_index::search::{CancelToken, Query, SearchOptions};
//! use strata_index::{IndexBuilder, IndexOptions};
//!
//! let root = FileRef::from_parts(5, 5);
//! let rec = |n: u64, name: &str, dir: bool| ScanRecord {
//!     id: if n == 5 { root } else { FileRef::from_parts(n, 1) },
//!     links: vec![NameLink { parent: root, name: WideName::from_str_lossless(name) }],
//!     attributes: 0,
//!     flags: if dir { EntryFlags::DIR } else { EntryFlags::EMPTY },
//!     times: Times::default(),
//!     fn_created: None,
//!     sizes: Sizes { logical: n, allocated: n, ..Sizes::default() },
//!     reparse: None,
//!     ads: vec![],
//! };
//! let mut b = IndexBuilder::new(IndexOptions::default());
//! b.push(rec(5, "", true))?;
//! b.push(rec(40, "llama-3-8b.Q4.gguf", false))?;
//! b.push(rec(41, "notes.txt", false))?;
//! let index = b.finish()?;
//!
//! let q = Query::parse("*.gguf", FileTime(0)).unwrap();
//! let streamed = Mutex::new(0);
//! let out = index
//!     .search(&q, &SearchOptions::default(), &CancelToken::new(), &|batch| {
//!         *streamed.lock().unwrap() += batch.len();
//!     })
//!     .unwrap();
//! assert_eq!(out.hits.len(), 1);
//! assert_eq!(index.name_lossy(out.hits[0].id), "llama-3-8b.Q4.gguf");
//! assert_eq!(*streamed.lock().unwrap(), 1);
//! # Ok::<(), strata_index::IndexError>(())
//! ```

mod matcher;
pub mod parse;

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use strata_core::{EntryFlags, Safety, SizeMode};

pub use parse::{Query, QueryError, SafetyFilter, Term};

use crate::fold;
use crate::index::{DEAD, EntryId, Index};
use crate::names::SAMPLE;
use matcher::Matcher;

/// Safety tier provider (the classifier lives in another crate).
pub type SafetyFn<'a> = dyn Fn(EntryId) -> Safety + Sync + 'a;

/// App-name provider: whether app `id`'s name matches the (lowercased)
/// `app:` value.
pub type AppMatchFn<'a> = dyn Fn(u32, &str) -> bool + Sync + 'a;

/// Cancels a running search from another thread.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Chunks already running finish; no new chunk
    /// starts and no further batches are delivered.
    pub fn cancel(&self) {
        self.0.store(true, AtomicOrdering::Relaxed);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Relaxed)
    }
}

/// Final ordering of hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchSort {
    /// Best name match first (exact, prefix, word start, substring), then
    /// size.
    #[default]
    Relevance,
    /// Largest first.
    Size,
}

/// Search parameters.
#[derive(Clone, Copy)]
pub struct SearchOptions<'a> {
    /// Maximum hits returned (streamed batches are not capped).
    pub limit: usize,
    /// Final ordering.
    pub sort: SearchSort,
    /// Size mode for `size:` and size ordering.
    pub mode: SizeMode,
    /// Entries per parallel chunk (rounded up to a multiple of 16).
    pub chunk: usize,
    /// Safety provider for `safe:`; without one, `safe:` matches nothing.
    pub safety: Option<&'a SafetyFn<'a>>,
    /// App provider for `app:`; without one, `app:` matches nothing.
    pub app_matches: Option<&'a AppMatchFn<'a>>,
}

impl Default for SearchOptions<'_> {
    fn default() -> Self {
        Self {
            limit: 1000,
            sort: SearchSort::Relevance,
            mode: SizeMode::Allocated,
            chunk: 16 * 1024,
            safety: None,
            app_matches: None,
        }
    }
}

impl std::fmt::Debug for SearchOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchOptions")
            .field("limit", &self.limit)
            .field("sort", &self.sort)
            .field("mode", &self.mode)
            .field("chunk", &self.chunk)
            .field("safety", &self.safety.is_some())
            .field("app_matches", &self.app_matches.is_some())
            .finish()
    }
}

/// One search result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// Matching entry.
    pub id: EntryId,
    /// Relevance (higher is better).
    pub score: u32,
    /// Display size in the search's mode.
    pub size: u64,
}

/// Result of [`Index::search`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOutcome {
    /// Best hits, sorted, at most `limit`.
    pub hits: Vec<Hit>,
    /// Total matches found (before the limit).
    pub matched: u64,
    /// Whether the search was cancelled before scanning everything.
    pub cancelled: bool,
}

/// A query compiled against one index.
struct Compiled<'q> {
    q: &'q Query,
    matchers: Vec<Matcher>,
    needs_raw: bool,
    filter: crate::query::Filter,
    /// Folded `ext:` values with no id of their own because the extension
    /// table was full; entries sharing the overflow id are matched by name.
    overflow_exts: Vec<Vec<u8>>,
}

impl Index {
    /// Runs `query`, calling `on_batch` (possibly concurrently, from worker
    /// threads) with each chunk's matches as soon as the chunk is scanned.
    ///
    /// # Errors
    ///
    /// [`QueryError::BadRegex`] if a regex term fails to compile (terms built
    /// by hand rather than by [`Query::parse`]).
    pub fn search(
        &self,
        query: &Query,
        opts: &SearchOptions<'_>,
        cancel: &CancelToken,
        on_batch: &(dyn Fn(&[Hit]) + Sync),
    ) -> Result<SearchOutcome, QueryError> {
        let Some(c) = self.compile_query(query, opts)? else {
            return Ok(SearchOutcome::default());
        };
        let limit = opts.limit.max(1);
        let matched = AtomicU64::new(0);
        let chunk = opts.chunk.max(SAMPLE).div_ceil(SAMPLE) * SAMPLE;
        let base = self.names.base_len as usize;
        let n_chunks = base.div_ceil(chunk);

        let scan = |hits: &mut Vec<Hit>, id: u32, raw: &[u8], buf: &mut Vec<u8>| {
            if let Some(h) = self.eval(&c, opts, id, raw, buf) {
                hits.push(h);
            }
        };
        let mut best: Vec<Hit> = (0..n_chunks)
            .into_par_iter()
            .map(|k| {
                if cancel.is_cancelled() {
                    return Vec::new();
                }
                let start = (k * chunk) as u32;
                let end = ((k + 1) * chunk).min(base) as u32;
                let mut hits = Vec::new();
                let mut buf = Vec::with_capacity(256);
                self.names
                    .for_each_base(start, end, |id, raw| scan(&mut hits, id, raw, &mut buf));
                self.deliver(hits, &matched, cancel, on_batch, opts, limit)
            })
            .reduce(Vec::new, |a, b| merge_top(a, b, opts.sort, limit));

        if !cancel.is_cancelled() {
            let mut hits = Vec::new();
            let mut buf = Vec::with_capacity(256);
            for (id, raw) in self.names.moved_iter() {
                scan(&mut hits, id, raw, &mut buf);
            }
            let extra = self.deliver(hits, &matched, cancel, on_batch, opts, limit);
            best = merge_top(best, extra, opts.sort, limit);
        }
        best.sort_unstable_by(|a, b| cmp_hits(a, b, opts.sort));
        best.truncate(limit);
        Ok(SearchOutcome {
            hits: best,
            matched: matched.load(AtomicOrdering::Relaxed),
            cancelled: cancel.is_cancelled(),
        })
    }

    /// Streams a chunk's hits, then trims them to the top `limit`.
    fn deliver(
        &self,
        mut hits: Vec<Hit>,
        matched: &AtomicU64,
        cancel: &CancelToken,
        on_batch: &(dyn Fn(&[Hit]) + Sync),
        opts: &SearchOptions<'_>,
        limit: usize,
    ) -> Vec<Hit> {
        if hits.is_empty() {
            return hits;
        }
        matched.fetch_add(hits.len() as u64, AtomicOrdering::Relaxed);
        if !cancel.is_cancelled() {
            on_batch(&hits);
        }
        trim(&mut hits, opts.sort, limit);
        hits
    }

    /// Resolves index-specific parts of the query; `None` if it cannot match
    /// anything on this index.
    fn compile_query<'q>(
        &self,
        q: &'q Query,
        opts: &SearchOptions<'_>,
    ) -> Result<Option<Compiled<'q>>, QueryError> {
        if let Some(v) = q.volume {
            let mine = self
                .opts
                .volume
                .prefix
                .chars()
                .next()
                .map(|c| c.to_ascii_uppercase());
            if mine != Some(v) {
                return Ok(None);
            }
        }
        let mut filter = q.filter.clone();
        filter.size_mode = opts.mode;
        let mut overflow_exts = Vec::new();
        if !q.ext_names.is_empty() {
            filter.exts = Vec::new();
            for e in &q.ext_names {
                let folded = fold::fold(e.as_bytes());
                match self.exts.lookup(&folded) {
                    Some(id) => filter.exts.push(id),
                    None if self.exts.is_full() => overflow_exts.push(folded),
                    None => {}
                }
            }
            if !overflow_exts.is_empty() {
                filter.exts.push(crate::ext::EXT_OVERFLOW);
            }
            if filter.exts.is_empty() {
                return Ok(None);
            }
        }
        if (q.safety.is_some() && opts.safety.is_none())
            || (!q.apps.is_empty() && opts.app_matches.is_none())
        {
            return Ok(None);
        }
        let matchers = q
            .terms
            .iter()
            .map(|t| Matcher::compile(t, q.case_sensitive))
            .collect::<Result<Vec<_>, _>>()?;
        let needs_raw = matchers.iter().any(Matcher::wants_raw);
        Ok(Some(Compiled {
            q,
            matchers,
            needs_raw,
            filter,
            overflow_exts,
        }))
    }

    #[inline]
    fn eval(
        &self,
        c: &Compiled<'_>,
        opts: &SearchOptions<'_>,
        id: u32,
        raw: &[u8],
        buf: &mut Vec<u8>,
    ) -> Option<Hit> {
        if self.col.parent[id as usize] == DEAD || !c.filter.matches_u32(self, id) {
            return None;
        }
        if !c.overflow_exts.is_empty()
            && self.col.ext_id[id as usize] == crate::ext::EXT_OVERFLOW
            && !crate::ext::extension_of(raw)
                .is_some_and(|e| c.overflow_exts.contains(&fold::fold(e)))
        {
            return None;
        }
        if !c.q.reparse_kinds.is_empty()
            && !c.q.reparse_kinds.contains(&self.col.flags(id).reparse())
        {
            return None;
        }
        let mut score = 0u32;
        if !c.matchers.is_empty() {
            let name: &[u8] = if c.q.case_sensitive {
                raw
            } else if c.needs_raw && c.matchers.len() == 1 {
                &[]
            } else {
                fold::fold_into(raw, buf);
                buf
            };
            for m in &c.matchers {
                score += m.score(name, raw)?;
                if let Some(parts) = m.path_components()
                    && !self.ancestors_match(id, parts, c.q.case_sensitive)
                {
                    return None;
                }
            }
        }
        if let Some(sf) = c.q.safety {
            let tier = (opts.safety?)(EntryId(id));
            let ok = match sf {
                SafetyFilter::Tier(t) => tier == t,
                SafetyFilter::NotSafe => tier != Safety::Safe,
            };
            if !ok {
                return None;
            }
        }
        if !c.q.apps.is_empty() {
            let app = self.col.owner_app[id as usize];
            let f = opts.app_matches?;
            if app == 0 || !c.q.apps.iter().any(|a| f(app, a)) {
                return None;
            }
        }
        if self.col.has(id, EntryFlags::DIR) {
            score += 1;
        }
        Some(Hit {
            id: EntryId(id),
            score,
            size: self.size(EntryId(id), opts.mode),
        })
    }

    /// Whether the ancestors of `id` match every path component but the last
    /// (innermost first).
    fn ancestors_match(&self, id: u32, parts: &[matcher::Component], case_sensitive: bool) -> bool {
        let mut cur = id;
        let mut buf = Vec::new();
        for part in parts.iter().rev().skip(1) {
            let p = self.col.parent[cur as usize];
            if p >= DEAD {
                return false;
            }
            cur = p;
            let raw = self.names.get(cur);
            let name: &[u8] = if case_sensitive {
                raw
            } else {
                fold::fold_into(raw, &mut buf);
                &buf
            };
            if !Matcher::component_matches(part, name) {
                return false;
            }
        }
        true
    }
}

fn cmp_hits(a: &Hit, b: &Hit, sort: SearchSort) -> Ordering {
    match sort {
        SearchSort::Relevance => b.score.cmp(&a.score).then(b.size.cmp(&a.size)),
        SearchSort::Size => b.size.cmp(&a.size).then(b.score.cmp(&a.score)),
    }
    .then(a.id.cmp(&b.id))
}

fn trim(v: &mut Vec<Hit>, sort: SearchSort, limit: usize) {
    if v.len() > limit {
        v.select_nth_unstable_by(limit, |a, b| cmp_hits(a, b, sort));
        v.truncate(limit);
    }
}

fn merge_top(mut a: Vec<Hit>, mut b: Vec<Hit>, sort: SearchSort, limit: usize) -> Vec<Hit> {
    if a.len() < b.len() {
        std::mem::swap(&mut a, &mut b);
    }
    a.extend(b);
    trim(&mut a, sort, limit);
    a
}
