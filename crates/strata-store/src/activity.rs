//! ETW activity rollups and "last writer" per path.
//!
//! The tracer aggregates file events in memory and periodically flushes
//! [`ActivitySample`]s; samples for the same (hour, process image, directory)
//! are summed, so flushing more often than hourly is fine. Directories are
//! keyed by [`path_hash`](crate::path_hash). Process images are stored once
//! in `process_images`.
//!
//! Activity data is private and local: [`Store::clear_activity`] removes all
//! of it in one call, and [`Store::prune_activity`] enforces the retention
//! window (default 30 days, `activity.retention_days`).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::error::Result;
use crate::{Store, i2u, u2i};

/// Activity of one process image in one directory, to be added to the hour
/// containing `at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySample {
    /// Any instant inside the hour the counts belong to.
    pub at: Timestamp,
    /// Full image path of the process.
    pub image: String,
    /// [`path_hash`](crate::path_hash) of the directory written to.
    pub dir_hash: u64,
    /// Bytes written.
    pub bytes_written: u64,
    /// Files created.
    pub files_created: u64,
    /// Files deleted.
    pub files_deleted: u64,
}

/// Totals for one process image over a time window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterTotal {
    /// Full image path.
    pub image: String,
    /// Bytes written.
    pub bytes_written: u64,
    /// Files created.
    pub files_created: u64,
    /// Files deleted.
    pub files_deleted: u64,
}

/// The most recent known writer of a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastWrite {
    /// [`path_hash`](crate::path_hash) of the file or directory.
    pub path_hash: u64,
    /// Full image path of the writer.
    pub image: String,
    /// Process id at the time, if known (ids are reused; informational).
    pub pid: Option<u32>,
    /// When it wrote.
    pub at: Timestamp,
}

/// What [`Store::prune_activity`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ActivityPruneReport {
    /// Hourly rows deleted.
    pub hourly_rows: u64,
    /// Last-writer rows deleted.
    pub last_writer_rows: u64,
}

const SQL_INSERT_IMAGE: &str = "
INSERT INTO process_images (path) VALUES (?1)
ON CONFLICT (path) DO NOTHING";

const SQL_SELECT_IMAGE: &str = "
SELECT id FROM process_images WHERE path = ?1";

const SQL_ADD_HOURLY: &str = "
INSERT INTO activity_hourly (hour, image_id, dir_hash, bytes_written, files_created, files_deleted)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT (hour, image_id, dir_hash) DO UPDATE SET
    bytes_written = bytes_written + excluded.bytes_written,
    files_created = files_created + excluded.files_created,
    files_deleted = files_deleted + excluded.files_deleted";

const SQL_TOP_WRITERS: &str = "
SELECT p.path, sum(a.bytes_written), sum(a.files_created), sum(a.files_deleted)
FROM activity_hourly AS a
JOIN process_images AS p ON p.id = a.image_id
WHERE a.hour >= ?1
GROUP BY a.image_id
ORDER BY sum(a.bytes_written) DESC, p.path
LIMIT ?2";

const SQL_DIR_WRITERS: &str = "
SELECT p.path, sum(a.bytes_written), sum(a.files_created), sum(a.files_deleted)
FROM activity_hourly AS a
JOIN process_images AS p ON p.id = a.image_id
WHERE a.dir_hash = ?1 AND a.hour >= ?2
GROUP BY a.image_id
ORDER BY sum(a.bytes_written) DESC, p.path
LIMIT ?3";

const SQL_SET_LAST_WRITER: &str = "
INSERT INTO last_writer (path_hash, image_id, pid, written_at)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT (path_hash) DO UPDATE SET
    image_id = excluded.image_id,
    pid = excluded.pid,
    written_at = excluded.written_at
WHERE excluded.written_at >= last_writer.written_at";

const SQL_GET_LAST_WRITER: &str = "
SELECT p.path, l.pid, l.written_at
FROM last_writer AS l
JOIN process_images AS p ON p.id = l.image_id
WHERE l.path_hash = ?1";

const SQL_PRUNE_HOURLY: &str = "
DELETE FROM activity_hourly WHERE hour < ?1";

const SQL_PRUNE_LAST_WRITER: &str = "
DELETE FROM last_writer WHERE written_at < ?1";

const SQL_PRUNE_IMAGES: &str = "
DELETE FROM process_images
WHERE NOT EXISTS (SELECT 1 FROM activity_hourly WHERE image_id = process_images.id)
  AND NOT EXISTS (SELECT 1 FROM last_writer WHERE image_id = process_images.id)";

const SQL_CLEAR_ACTIVITY: &str = "
DELETE FROM activity_hourly;
DELETE FROM last_writer;
DELETE FROM process_images;";

fn image_id(conn: &Connection, cache: &mut HashMap<String, i64>, image: &str) -> Result<i64> {
    if let Some(id) = cache.get(image) {
        return Ok(*id);
    }
    conn.prepare_cached(SQL_INSERT_IMAGE)?.execute([image])?;
    let id: i64 = conn
        .prepare_cached(SQL_SELECT_IMAGE)?
        .query_row([image], |r| r.get(0))?;
    cache.insert(image.to_owned(), id);
    Ok(id)
}

fn writer_total(r: &rusqlite::Row<'_>) -> rusqlite::Result<WriterTotal> {
    Ok(WriterTotal {
        image: r.get(0)?,
        bytes_written: i2u(r.get(1)?),
        files_created: i2u(r.get(2)?),
        files_deleted: i2u(r.get(3)?),
    })
}

impl Store {
    /// Adds samples to their hourly rollups in one transaction.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_store::*;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// store.record_activity(&[ActivitySample {
    ///     at: Timestamp(1_760_000_000),
    ///     image: r"C:\Program Files\App\app.exe".into(),
    ///     dir_hash: path_hash(r"C:\Users\me\AppData\Local\App\Cache"),
    ///     bytes_written: 1 << 20, files_created: 3, files_deleted: 0,
    /// }]).unwrap();
    /// let top = store.top_writers(Timestamp(1_759_990_000), 10).unwrap();
    /// assert_eq!(top[0].bytes_written, 1 << 20);
    /// ```
    ///
    /// # Errors
    ///
    /// Database errors; nothing is added on error.
    pub fn record_activity(&self, samples: &[ActivitySample]) -> Result<()> {
        self.history().write(|tx| {
            let mut images = HashMap::new();
            let mut add = tx.prepare_cached(SQL_ADD_HOURLY)?;
            for s in samples {
                let image = image_id(tx, &mut images, &s.image)?;
                add.execute(params![
                    s.at.hour_start().0,
                    image,
                    u2i(s.dir_hash),
                    u2i(s.bytes_written),
                    u2i(s.files_created),
                    u2i(s.files_deleted),
                ])?;
            }
            Ok(())
        })
    }

    /// Top processes by bytes written since `since` (rounded down to the
    /// hour). "Today" and "last hour" are just different `since` values.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn top_writers(&self, since: Timestamp, limit: usize) -> Result<Vec<WriterTotal>> {
        self.history().read(|c| {
            let rows = c
                .prepare_cached(SQL_TOP_WRITERS)?
                .query_map(
                    params![
                        since.hour_start().0,
                        i64::try_from(limit).unwrap_or(i64::MAX)
                    ],
                    writer_total,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Processes that wrote into one directory since `since`, top first.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn dir_writers(
        &self,
        dir_hash: u64,
        since: Timestamp,
        limit: usize,
    ) -> Result<Vec<WriterTotal>> {
        self.history().read(|c| {
            let rows = c
                .prepare_cached(SQL_DIR_WRITERS)?
                .query_map(
                    params![
                        u2i(dir_hash),
                        since.hour_start().0,
                        i64::try_from(limit).unwrap_or(i64::MAX)
                    ],
                    writer_total,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Upserts last-writer rows. An older write never replaces a newer one,
    /// so out-of-order flushes are harmless.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn set_last_writers(&self, writes: &[LastWrite]) -> Result<()> {
        self.history().write(|tx| {
            let mut images = HashMap::new();
            let mut set = tx.prepare_cached(SQL_SET_LAST_WRITER)?;
            for w in writes {
                let image = image_id(tx, &mut images, &w.image)?;
                set.execute(params![
                    u2i(w.path_hash),
                    image,
                    w.pid.map(i64::from),
                    w.at.0
                ])?;
            }
            Ok(())
        })
    }

    /// The last known writer of a path ("Who touched this").
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn last_writer(&self, path_hash: u64) -> Result<Option<LastWrite>> {
        self.history().read(|c| {
            Ok(c.prepare_cached(SQL_GET_LAST_WRITER)?
                .query_row([u2i(path_hash)], |r| {
                    Ok(LastWrite {
                        path_hash,
                        image: r.get(0)?,
                        pid: r
                            .get::<_, Option<i64>>(1)?
                            .and_then(|p| u32::try_from(p).ok()),
                        at: Timestamp(r.get(2)?),
                    })
                })
                .optional()?)
        })
    }

    /// Deletes activity older than `retention_days` days before now.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn prune_activity(&self, retention_days: u32) -> Result<ActivityPruneReport> {
        let cutoff = self.now().minus_days(retention_days);
        self.history().write(|tx| {
            let hourly_rows = tx.execute(SQL_PRUNE_HOURLY, [cutoff.0])? as u64;
            let last_writer_rows = tx.execute(SQL_PRUNE_LAST_WRITER, [cutoff.0])? as u64;
            tx.execute(SQL_PRUNE_IMAGES, [])?;
            Ok(ActivityPruneReport {
                hourly_rows,
                last_writer_rows,
            })
        })
    }

    /// Deletes all activity data ("Clear activity data").
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn clear_activity(&self) -> Result<()> {
        self.history().write(|tx| {
            tx.execute_batch(SQL_CLEAR_ACTIVITY)?;
            Ok(())
        })
    }
}
