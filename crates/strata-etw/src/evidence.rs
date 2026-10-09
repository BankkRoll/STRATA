//! Attribution evidence for `strata-classify` ("this folder is only ever
//! written by `app.exe`").
//!
//! For each directory that was written to, the dominant writer's **share**
//! of the directory's activity score ([`Counts::score`]) is weighted by
//! **support**, the number of distinct hours it was active there, saturating
//! at [`EvidenceConfig::full_support_hours`]:
//!
//! ```text
//! weight = share × min(1, active_hours / full_support_hours)
//! ```
//!
//! Evidence is emitted only for the dominant image, only when its share is at
//! least [`EvidenceConfig::min_share`] and the directory saw at least
//! [`EvidenceConfig::min_score`] of activity. The weight is in `0..=1`;
//! `AppCatalog::add_evidence` treats an accumulated 1.0 as high confidence,
//! which this formula reaches only for a sole writer seen across enough
//! separate hours. A single burst (one install, one download) stays
//! heuristic.
//!
//! Evidence is recorded only for directories actually written to, never
//! propagated to ancestors: one app being the only writer under
//! `AppData\Local` during a window says nothing about who owns
//! `AppData\Local`. The classifier walks ancestors itself, so files below an
//! evidenced directory inherit it.
//!
//! `add_evidence` accumulates weights, so feed a freshly built catalog once
//! per evidence computation rather than re-adding the same feed.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::aggregate::Counts;

/// Activity of one image in one directory over the evidence period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirActivity {
    /// Directory (DOS path).
    pub dir: String,
    /// Full image path.
    pub image: String,
    /// Counters over the period.
    pub counts: Counts,
    /// Distinct hours with activity.
    pub active_hours: u32,
}

/// One `add_evidence(prefix, app, weight)` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// Directory prefix.
    pub prefix: String,
    /// Attribution label: the image file name (`app.exe`), which the
    /// classifier fuzzy-matches against installed apps.
    pub app: String,
    /// Full image path, for display.
    pub image: String,
    /// Weight in `0..=1`.
    pub weight: f32,
    /// The dominant writer's share of the directory's activity.
    pub share: f32,
}

/// Thresholds for [`evidence`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EvidenceConfig {
    /// Minimum share of the directory's activity (default 0.6).
    pub min_share: f32,
    /// Minimum total activity score in the directory (default 64 KiB).
    pub min_score: u64,
    /// Active hours at which support saturates (default 3).
    pub full_support_hours: u32,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            min_share: 0.6,
            min_score: 64 * 1024,
            full_support_hours: 3,
        }
    }
}

/// Derives evidence from per-directory activity.
///
/// # Example
///
/// ```
/// use strata_etw::aggregate::Counts;
/// use strata_etw::evidence::{evidence, DirActivity, EvidenceConfig};
/// let rows = [DirActivity {
///     dir: r"C:\Users\me\AppData\Local\Tool\cache".into(),
///     image: r"C:\Program Files\Tool\tool.exe".into(),
///     counts: Counts::write(10 << 20),
///     active_hours: 5,
/// }];
/// let ev = evidence(&rows, &EvidenceConfig::default());
/// assert_eq!(ev[0].app, "tool.exe");
/// assert_eq!(ev[0].weight, 1.0);
/// ```
#[must_use]
pub fn evidence(rows: &[DirActivity], cfg: &EvidenceConfig) -> Vec<Evidence> {
    let mut by_dir: HashMap<String, (u64, Vec<&DirActivity>)> = HashMap::new();
    for r in rows {
        let key = strata_store::normalize_path(&r.dir);
        let e = by_dir.entry(key).or_default();
        e.0 = e.0.saturating_add(r.counts.score());
        e.1.push(r);
    }
    let full = cfg.full_support_hours.max(1) as f32;
    let mut out = Vec::new();
    for (total, writers) in by_dir.into_values() {
        if total == 0 || total < cfg.min_score {
            continue;
        }
        // Several rows can share an image (spelling differences); merge.
        let mut per_image: HashMap<&str, (u64, u32, &str)> = HashMap::new();
        for w in writers {
            let e = per_image.entry(&w.image).or_insert((0, 0, &w.dir));
            e.0 = e.0.saturating_add(w.counts.score());
            e.1 = e.1.max(w.active_hours);
        }
        let Some((image, (score, hours, dir))) = per_image
            .into_iter()
            .max_by(|a, b| a.1.0.cmp(&b.1.0).then_with(|| b.0.cmp(a.0)))
        else {
            continue;
        };
        let share = (score as f64 / total as f64) as f32;
        if share < cfg.min_share {
            continue;
        }
        let support = (hours as f32 / full).min(1.0);
        let app = image.rsplit(['\\', '/']).next().unwrap_or(image);
        out.push(Evidence {
            prefix: dir.to_string(),
            app: app.to_string(),
            image: image.to_string(),
            weight: (share * support).clamp(0.0, 1.0),
            share,
        });
    }
    out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dir: &str, image: &str, bytes: u64, hours: u32) -> DirActivity {
        DirActivity {
            dir: dir.into(),
            image: image.into(),
            counts: Counts::write(bytes),
            active_hours: hours,
        }
    }

    const MB: u64 = 1 << 20;

    #[test]
    fn weights_follow_share_and_support() {
        let rows = [
            // Sole writer, many hours: full weight.
            row(r"C:\d\solo", r"C:\p\a.exe", 10 * MB, 6),
            // Sole writer, one burst: a third.
            row(r"C:\d\burst", r"C:\p\b.exe", 10 * MB, 1),
            // 75/25 split over 3 hours.
            row(r"C:\d\shared", r"C:\p\c.exe", 3 * MB, 3),
            row(r"C:\d\shared", r"C:\p\d.exe", MB, 3),
            // 50/50: no dominant writer.
            row(r"C:\d\even", r"C:\p\e.exe", MB, 3),
            row(r"C:\d\even", r"C:\p\f.exe", MB, 3),
            // Too little activity.
            row(r"C:\d\tiny", r"C:\p\g.exe", 1000, 9),
        ];
        let ev = evidence(&rows, &EvidenceConfig::default());
        let get = |p: &str| ev.iter().find(|e| e.prefix == p);
        assert_eq!(get(r"C:\d\solo").unwrap().weight, 1.0);
        assert_eq!(get(r"C:\d\solo").unwrap().app, "a.exe");
        assert!((get(r"C:\d\burst").unwrap().weight - 1.0 / 3.0).abs() < 1e-6);
        let s = get(r"C:\d\shared").unwrap();
        assert_eq!(s.app, "c.exe");
        assert!((s.weight - 0.75).abs() < 1e-6);
        assert!(get(r"C:\d\even").is_none());
        assert!(get(r"C:\d\tiny").is_none());
        assert_eq!(ev.len(), 3);
    }

    #[test]
    fn creates_and_deletes_count_toward_share() {
        let mut churn = row(r"C:\t", r"C:\p\churn.exe", 0, 3);
        churn.counts.files_created = 100;
        churn.counts.files_deleted = 100;
        let rows = [churn, row(r"c:\T", r"C:\p\writer.exe", 100 * 1024, 3)];
        let ev = evidence(&rows, &EvidenceConfig::default());
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].app, "churn.exe");
        assert!(ev[0].share > 0.85);
    }
}
