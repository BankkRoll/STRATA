//! Duplicate-finder hash cache.
//!
//! One row per (volume, file reference) holding the size and mtime the hashes
//! were computed for, the partial xxh3 and, once computed, the full BLAKE3.
//! A lookup only hits when size and mtime still match, so a stale row can
//! never produce a wrong duplicate verdict; live updates additionally call
//! [`Store::invalidate_hashes`] for changed file references to keep the table
//! small.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime};

use crate::error::Result;
use crate::snapshot::{VolumeKey, ensure_volume, find_volume};
use crate::{Store, i2u, u2i};

/// Identity of file content for cache purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HashKey {
    /// File reference on the volume.
    pub file_ref: FileRef,
    /// Logical size.
    pub size: u64,
    /// Last-write time.
    pub mtime: FileTime,
}

/// Cached hashes for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedHash {
    /// Content identity the hashes belong to.
    pub key: HashKey,
    /// xxh3 of the first, middle and last 64 KiB.
    pub partial: u64,
    /// Full BLAKE3, once computed.
    pub full: Option<[u8; 32]>,
}

const SQL_UPSERT_HASH: &str = "
INSERT INTO hash_cache (volume_id, file_ref, size, mtime, partial, full)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT (volume_id, file_ref) DO UPDATE SET
    size = excluded.size,
    mtime = excluded.mtime,
    partial = excluded.partial,
    full = CASE
        WHEN excluded.full IS NULL
         AND hash_cache.size = excluded.size
         AND hash_cache.mtime = excluded.mtime
         AND hash_cache.partial = excluded.partial
        THEN hash_cache.full
        ELSE excluded.full
    END";

const SQL_LOOKUP_HASH: &str = "
SELECT partial, full FROM hash_cache
WHERE volume_id = ?1 AND file_ref = ?2 AND size = ?3 AND mtime = ?4";

const SQL_INVALIDATE_HASH: &str = "
DELETE FROM hash_cache WHERE volume_id = ?1 AND file_ref = ?2";

const SQL_CLEAR_HASHES: &str = "
DELETE FROM hash_cache";

impl Store {
    /// Inserts or replaces cache rows in one transaction. Upserting only a
    /// partial hash for unchanged content keeps an existing full hash.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_core::{FileRef, FileTime};
    /// use strata_store::*;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// let volume = VolumeKey { serial: 7, guid_path: "v".into() };
    /// let key = HashKey { file_ref: FileRef(42), size: 1 << 30, mtime: FileTime(1) };
    /// store.upsert_hashes(&volume, &[CachedHash { key, partial: 0xFEED, full: None }]).unwrap();
    /// let hit = store.lookup_hashes(&volume, &[key]).unwrap();
    /// assert_eq!(hit[0].unwrap().partial, 0xFEED);
    /// ```
    ///
    /// # Errors
    ///
    /// Database errors; nothing is written on error.
    pub fn upsert_hashes(&self, volume: &VolumeKey, entries: &[CachedHash]) -> Result<()> {
        self.history().write(|tx| {
            let vid = ensure_volume(tx, volume)?;
            let mut upsert = tx.prepare_cached(SQL_UPSERT_HASH)?;
            for e in entries {
                upsert.execute(params![
                    vid,
                    u2i(e.key.file_ref.0),
                    u2i(e.key.size),
                    u2i(e.key.mtime.0),
                    u2i(e.partial),
                    e.full.as_ref().map(<[u8; 32]>::as_slice),
                ])?;
            }
            Ok(())
        })
    }

    /// Looks up many keys at once; each result is `None` on a miss or when
    /// size/mtime no longer match.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn lookup_hashes(
        &self,
        volume: &VolumeKey,
        keys: &[HashKey],
    ) -> Result<Vec<Option<CachedHash>>> {
        self.history().read(|c| {
            let Some(vid) = find_volume(c, volume)? else {
                return Ok(vec![None; keys.len()]);
            };
            let mut stmt = c.prepare_cached(SQL_LOOKUP_HASH)?;
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                let row: Option<(i64, Option<Vec<u8>>)> = stmt
                    .query_row(
                        params![vid, u2i(key.file_ref.0), u2i(key.size), u2i(key.mtime.0)],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                out.push(row.map(|(partial, full)| CachedHash {
                    key: *key,
                    partial: i2u(partial),
                    // The CHECK constraint guarantees 32 bytes; anything else
                    // is treated as "not computed" rather than an error.
                    full: full.and_then(|f| <[u8; 32]>::try_from(f.as_slice()).ok()),
                }));
            }
            Ok(out)
        })
    }

    /// Drops cache rows for changed or deleted files (called from live
    /// updates).
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn invalidate_hashes(&self, volume: &VolumeKey, refs: &[FileRef]) -> Result<u64> {
        self.history().write(|tx| {
            let Some(vid) = find_volume(tx, volume)? else {
                return Ok(0);
            };
            let mut delete = tx.prepare_cached(SQL_INVALIDATE_HASH)?;
            let mut n = 0;
            for r in refs {
                n += delete.execute(params![vid, u2i(r.0)])? as u64;
            }
            Ok(n)
        })
    }

    /// Empties the hash cache ("Clear caches").
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn clear_hash_cache(&self) -> Result<()> {
        self.history().write(|tx| {
            tx.execute(SQL_CLEAR_HASHES, [])?;
            Ok(())
        })
    }
}
