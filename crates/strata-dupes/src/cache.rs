//! The hash cache contract.
//!
//! Types mirror `strata_store::{HashKey, CachedHash}` field for field, and
//! [`HashCache`] mirrors `Store::{lookup_hashes, upsert_hashes,
//! invalidate_hashes}`, so the app's adapter is a one-line forward per
//! method. Keys are what a handle showed (file reference, unnamed-stream
//! size, full-precision last-write time), so a lookup can only hit for
//! unchanged content.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime};

use crate::candidate::VolumeKey;

/// Identity of file content for cache purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HashKey {
    /// File reference on the volume.
    pub file_ref: FileRef,
    /// Logical size of the unnamed stream.
    pub size: u64,
    /// Last-write time.
    pub mtime: FileTime,
}

/// Cached hashes for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedHash {
    /// Content identity the hashes belong to.
    pub key: HashKey,
    /// xxh3 of the first, middle and last 64 KiB (and the size).
    pub partial: u64,
    /// Full BLAKE3, once computed.
    pub full: Option<[u8; 32]>,
}

/// A cache backend failure. The scan treats it as a miss and keeps going.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("hash cache: {0}")]
pub struct CacheError(pub String);

/// Persistent hash cache keyed by (volume, file reference), implemented by
/// `strata-store` in the app.
///
/// Contract (as the store implements it): a lookup hits only when size and
/// mtime equal the stored row; an upsert with `full: None` for unchanged
/// content keeps an existing full hash; invalidation drops rows by file
/// reference whatever their size or mtime.
pub trait HashCache: Send + Sync {
    /// Looks up many keys; results are index-aligned with `keys`.
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn lookup(
        &self,
        volume: &VolumeKey,
        keys: &[HashKey],
    ) -> Result<Vec<Option<CachedHash>>, CacheError>;

    /// Inserts or replaces rows.
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn upsert(&self, volume: &VolumeKey, entries: &[CachedHash]) -> Result<(), CacheError>;

    /// Drops rows for changed or deleted files (called from live updates).
    /// Returns how many rows were removed.
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn invalidate(&self, volume: &VolumeKey, refs: &[FileRef]) -> Result<u64, CacheError>;
}

/// In-process [`HashCache`] with the store's semantics. Useful before the
/// store is wired up, and for one-shot scans that need no persistence.
#[derive(Debug, Clone, Default)]
pub struct MemoryHashCache {
    rows: Arc<Mutex<HashMap<RowKey, CachedHash>>>,
}

impl MemoryHashCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the cache holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Rows that have a full hash.
    #[must_use]
    pub fn full_count(&self) -> usize {
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|r| r.full.is_some())
            .count()
    }
}

type RowKey = (u64, Arc<str>, FileRef);

fn row_key(volume: &VolumeKey, r: FileRef) -> RowKey {
    (volume.serial, Arc::clone(&volume.guid_path), r)
}

impl HashCache for MemoryHashCache {
    fn lookup(
        &self,
        volume: &VolumeKey,
        keys: &[HashKey],
    ) -> Result<Vec<Option<CachedHash>>, CacheError> {
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(keys
            .iter()
            .map(|k| {
                rows.get(&row_key(volume, k.file_ref))
                    .filter(|r| r.key.size == k.size && r.key.mtime == k.mtime)
                    .copied()
            })
            .collect())
    }

    fn upsert(&self, volume: &VolumeKey, entries: &[CachedHash]) -> Result<(), CacheError> {
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        for e in entries {
            let slot = rows.entry(row_key(volume, e.key.file_ref)).or_insert(*e);
            let keep_full = e.full.is_none() && slot.key == e.key && slot.partial == e.partial;
            let full = if keep_full { slot.full } else { e.full };
            *slot = CachedHash { full, ..*e };
        }
        Ok(())
    }

    fn invalidate(&self, volume: &VolumeKey, refs: &[FileRef]) -> Result<u64, CacheError> {
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(refs
            .iter()
            .filter(|r| rows.remove(&row_key(volume, **r)).is_some())
            .count() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_cache_has_store_semantics() {
        let c = MemoryHashCache::new();
        let v = VolumeKey::new(1, "v");
        let k = HashKey {
            file_ref: FileRef(9),
            size: 10,
            mtime: FileTime(5),
        };
        c.upsert(
            &v,
            &[CachedHash {
                key: k,
                partial: 1,
                full: Some([7; 32]),
            }],
        )
        .unwrap();
        // Partial-only upsert of unchanged content keeps the full hash.
        c.upsert(
            &v,
            &[CachedHash {
                key: k,
                partial: 1,
                full: None,
            }],
        )
        .unwrap();
        assert_eq!(c.lookup(&v, &[k]).unwrap()[0].unwrap().full, Some([7; 32]));
        // A different mtime misses, and replacing the row drops the full hash.
        let k2 = HashKey {
            mtime: FileTime(6),
            ..k
        };
        assert!(c.lookup(&v, &[k2]).unwrap()[0].is_none());
        c.upsert(
            &v,
            &[CachedHash {
                key: k2,
                partial: 1,
                full: None,
            }],
        )
        .unwrap();
        assert_eq!(c.lookup(&v, &[k2]).unwrap()[0].unwrap().full, None);
        // Other volumes are separate.
        assert!(c.lookup(&VolumeKey::new(2, "v"), &[k2]).unwrap()[0].is_none());
        assert_eq!(c.invalidate(&v, &[FileRef(9), FileRef(10)]).unwrap(), 1);
        assert!(c.is_empty());
    }
}
