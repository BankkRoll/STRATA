//! Snapshot retention: age limit plus weekly thinning.
//!
//! Policy (default values in parentheses): snapshots older than `keep_days` (90) are
//! deleted; snapshots older than `thin_after_days` (30) are thinned so only
//! the last snapshot of each ISO week (Monday-Sunday, UTC) survives. "Last of
//! the week" is judged over all of the volume's snapshots, so in a week that
//! straddles the thinning cutoff the older days are dropped in favour of the
//! week's newest snapshot, which is still inside the recent window.
//!
//! The newest snapshot of each volume is always kept, however old, so the
//! "since last scan" banner keeps a baseline after a long break.
//!
//! Everything runs in one transaction: a crash leaves either the old or the
//! fully thinned history. Deleting snapshots then garbage-collects `paths`
//! rows no remaining snapshot references.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::Store;
use crate::clock::Timestamp;
use crate::error::Result;
use crate::snapshot::for_each_referenced_path;

/// Retention configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Delete snapshots older than this many days. 0 keeps them forever.
    pub keep_days: u32,
    /// Thin snapshots older than this many days to one per ISO week. 0
    /// disables thinning.
    pub thin_after_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_days: 90,
            thin_after_days: 30,
        }
    }
}

/// What [`Store::apply_retention`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RetentionReport {
    /// Snapshots deleted.
    pub deleted_snapshots: u64,
    /// Path rows garbage-collected.
    pub deleted_paths: u64,
}

const SQL_ALL_SNAPSHOTS: &str = "
SELECT id, volume_id, taken_at FROM snapshots ORDER BY volume_id, taken_at, id";

const SQL_DELETE_SNAPSHOT: &str = "
DELETE FROM snapshots WHERE id = ?1";

const SQL_ALL_PATH_IDS: &str = "
SELECT id FROM paths";

const SQL_DELETE_PATH: &str = "
DELETE FROM paths WHERE id = ?1";

#[derive(Clone, Copy)]
struct Snap {
    id: i64,
    volume: i64,
    at: Timestamp,
}

/// Ids to delete under `policy` at `now`. Pure, so the rules are testable
/// without a database.
fn select_doomed(snaps: &[Snap], policy: &RetentionPolicy, now: Timestamp) -> Vec<i64> {
    let keep_cutoff = (policy.keep_days > 0).then(|| now.minus_days(policy.keep_days));
    let thin_cutoff = (policy.thin_after_days > 0).then(|| now.minus_days(policy.thin_after_days));

    let mut by_volume: HashMap<i64, Vec<Snap>> = HashMap::new();
    for s in snaps {
        by_volume.entry(s.volume).or_default().push(*s);
    }
    let mut doomed = Vec::new();
    for list in by_volume.values() {
        let newest = list.iter().max_by_key(|s| (s.at, s.id)).map(|s| s.id);
        let mut week_last: HashMap<i64, (Timestamp, i64)> = HashMap::new();
        for s in list {
            let slot = week_last
                .entry(s.at.iso_week_index())
                .or_insert((s.at, s.id));
            if (s.at, s.id) > *slot {
                *slot = (s.at, s.id);
            }
        }
        for s in list {
            if Some(s.id) == newest {
                continue;
            }
            let expired = keep_cutoff.is_some_and(|c| s.at < c);
            let thinned = thin_cutoff.is_some_and(|c| s.at < c)
                && week_last.get(&s.at.iso_week_index()).map(|w| w.1) != Some(s.id);
            if expired || thinned {
                doomed.push(s.id);
            }
        }
    }
    doomed.sort_unstable();
    doomed
}

impl Store {
    /// Applies `policy` at the store clock's current time.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_store::*;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// let report = store.apply_retention(&RetentionPolicy::default()).unwrap();
    /// assert_eq!(report.deleted_snapshots, 0);
    /// ```
    ///
    /// # Errors
    ///
    /// Database errors; nothing is deleted on error.
    pub fn apply_retention(&self, policy: &RetentionPolicy) -> Result<RetentionReport> {
        let now = self.now();
        self.history().write(|tx| {
            let snaps = tx
                .prepare(SQL_ALL_SNAPSHOTS)?
                .query_map([], |r| {
                    Ok(Snap {
                        id: r.get(0)?,
                        volume: r.get(1)?,
                        at: Timestamp(r.get(2)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let doomed = select_doomed(&snaps, policy, now);
            if doomed.is_empty() {
                return Ok(RetentionReport::default());
            }
            let mut delete = tx.prepare_cached(SQL_DELETE_SNAPSHOT)?;
            for id in &doomed {
                delete.execute([id])?;
            }

            let mut referenced = HashSet::new();
            for_each_referenced_path(tx, |id| {
                referenced.insert(id);
            })?;
            let all_paths = tx
                .prepare(SQL_ALL_PATH_IDS)?
                .query_map([], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut delete_path = tx.prepare_cached(SQL_DELETE_PATH)?;
            let mut deleted_paths = 0;
            for id in all_paths.into_iter().filter(|id| !referenced.contains(id)) {
                deleted_paths += delete_path.execute([id])? as u64;
            }
            Ok(RetentionReport {
                deleted_snapshots: doomed.len() as u64,
                deleted_paths,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: u32, d: u32, h: u32) -> Timestamp {
        Timestamp::from_utc(y, m, d, h, 0, 0).unwrap()
    }

    fn snaps(times: &[Timestamp]) -> Vec<Snap> {
        times
            .iter()
            .enumerate()
            .map(|(i, t)| Snap {
                id: i as i64 + 1,
                volume: 1,
                at: *t,
            })
            .collect()
    }

    #[test]
    fn daily_snapshots_thin_to_weekly_then_expire() {
        let now = at(2026, 10, 9, 12);
        let times: Vec<_> = (0..120).map(|d| now.minus_days(d)).collect();
        let s = snaps(&times);
        let doomed: HashSet<_> = select_doomed(&s, &RetentionPolicy::default(), now)
            .into_iter()
            .collect();
        let kept: Vec<_> = s.iter().filter(|x| !doomed.contains(&x.id)).collect();
        // Days 0..=30 are recent and all kept (day 30 is exactly at the cutoff).
        for d in 0..=30 {
            assert!(kept.iter().any(|k| k.at == now.minus_days(d)), "day {d}");
        }
        for k in &kept {
            assert!(k.at >= now.minus_days(90), "expired {} kept", k.at);
        }
        let old: Vec<_> = kept.iter().filter(|k| k.at < now.minus_days(30)).collect();
        let weeks: HashSet<_> = old.iter().map(|k| k.at.iso_week_index()).collect();
        assert_eq!(weeks.len(), old.len(), "one per week");
        for k in &old {
            // Each surviving old snapshot is its week's Sunday (last day).
            assert_eq!((k.at.days_since_epoch() + 3).rem_euclid(7), 6, "{}", k.at);
        }
    }

    #[test]
    fn newest_snapshot_survives_any_age() {
        let now = at(2026, 10, 9, 0);
        let s = snaps(&[now.minus_days(400), now.minus_days(300)]);
        assert_eq!(select_doomed(&s, &RetentionPolicy::default(), now), vec![1]);
    }

    #[test]
    fn utc_week_boundary_is_respected_across_dst() {
        // EU DST ended 2026-10-25 01:00 UTC (a Sunday). The two snapshots are
        // 1 hour apart in UTC but in different ISO weeks.
        let sunday = Timestamp::from_utc(2026, 10, 25, 23, 30, 0).unwrap();
        let monday = Timestamp::from_utc(2026, 10, 26, 0, 30, 0).unwrap();
        let earlier_sunday = Timestamp::from_utc(2026, 10, 25, 0, 30, 0).unwrap();
        let newest = Timestamp::from_utc(2026, 12, 31, 0, 0, 0).unwrap();
        let now = Timestamp::from_utc(2027, 1, 1, 0, 0, 0).unwrap();
        let s = snaps(&[earlier_sunday, sunday, monday, newest]);
        let doomed = select_doomed(&s, &RetentionPolicy::default(), now);
        assert_eq!(doomed, vec![1], "only the earlier Sunday shares a week");
    }

    #[test]
    fn year_end_week_counts_once() {
        let now = Timestamp::from_utc(2027, 3, 1, 0, 0, 0).unwrap();
        let s = snaps(&[
            Timestamp::from_utc(2026, 12, 29, 0, 0, 0).unwrap(),
            Timestamp::from_utc(2027, 1, 2, 0, 0, 0).unwrap(),
            Timestamp::from_utc(2027, 1, 4, 0, 0, 0).unwrap(),
            now,
        ]);
        assert_eq!(select_doomed(&s, &RetentionPolicy::default(), now), vec![1]);
    }

    #[test]
    fn future_and_ties_are_safe() {
        let now = at(2026, 10, 9, 0);
        let future = now.0 + 86_400 * 10;
        let mut s = snaps(&[Timestamp(future), now.minus_days(40), now.minus_days(40)]);
        s[2].id = 3;
        let doomed = select_doomed(&s, &RetentionPolicy::default(), now);
        // Two snapshots at the same instant in the same week: the higher id wins.
        assert_eq!(doomed, vec![2]);
    }

    #[test]
    fn zero_disables_rules() {
        let now = at(2026, 10, 9, 0);
        let s = snaps(&[now.minus_days(1000), now.minus_days(999), now]);
        let policy = RetentionPolicy {
            keep_days: 0,
            thin_after_days: 0,
        };
        assert!(select_doomed(&s, &policy, now).is_empty());
    }

    #[test]
    fn volumes_are_independent() {
        let now = at(2026, 10, 9, 0);
        let s = vec![
            Snap {
                id: 1,
                volume: 1,
                at: now.minus_days(200),
            },
            Snap {
                id: 2,
                volume: 2,
                at: now.minus_days(1),
            },
        ];
        assert!(select_doomed(&s, &RetentionPolicy::default(), now).is_empty());
    }
}
