//! Lifecycle and connections for one SQLite database file.
//!
//! Responsibilities:
//! - Open/create with WAL, `foreign_keys=ON`, a busy timeout and the
//!   per-database `synchronous` level.
//! - Run `PRAGMA quick_check` and the embedded migrations on open.
//! - Hand out one serialized writer and a small pool of WAL readers.
//! - Turn `SQLITE_CORRUPT` / `SQLITE_NOTADB` from any statement into
//!   [`StoreError::Corrupt`] and remember it, so later calls fail fast instead
//!   of hammering a damaged file.
//! - Move a damaged file aside and start fresh ([`Db::reset`]).
//!
//! Threading: the state sits behind an `RwLock`. Every operation holds the
//! read side for its whole duration; only `reset` takes the write side. That
//! guarantees no connection is open while the file is renamed, which Windows
//! requires (SQLite opens files without `FILE_SHARE_DELETE`).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::clock::Timestamp;
use crate::error::{DbKind, Result, StoreError, is_corruption};
use crate::schema::{HISTORY_MIGRATIONS, Migration, STATE_MIGRATIONS, latest_version};

/// How long a statement waits on a lock held by another connection or
/// process before failing with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Idle reader connections kept per database. More readers than this can be
/// open at once; extras are closed when returned.
const MAX_IDLE_READERS: usize = 4;

/// Static description of one database file.
#[derive(Debug)]
pub(crate) struct DbSpec {
    pub kind: DbKind,
    pub migrations: &'static [Migration],
    /// `PRAGMA synchronous` value.
    pub synchronous: &'static str,
}

/// History data is derived and rebuildable, so `NORMAL` (WAL fsynced only at
/// checkpoints) is enough: a power cut can lose the last snapshot, never
/// corrupt the file.
pub(crate) const HISTORY_SPEC: DbSpec = DbSpec {
    kind: DbKind::History,
    migrations: HISTORY_MIGRATIONS,
    synchronous: "NORMAL",
};

/// The undo log is write-ahead for deletes: a record must be on disk before
/// the delete it describes happens, so every commit fsyncs (`FULL`). Writes
/// here are small and rare.
pub(crate) const STATE_SPEC: DbSpec = DbSpec {
    kind: DbKind::State,
    migrations: STATE_MIGRATIONS,
    synchronous: "FULL",
};

/// Health of one database file, as reported by
/// [`Store::health`](crate::Store::health).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbHealth {
    /// Open and usable.
    Ok,
    /// Damaged; operations fail with [`StoreError::Corrupt`] until reset.
    Corrupt {
        /// SQLite's description of the damage.
        detail: String,
    },
    /// Written by a newer schema; left untouched.
    TooNew {
        /// Version in the file.
        found: i64,
        /// Latest version this build supports.
        supported: i64,
    },
    /// Could not be opened for another reason.
    Unavailable {
        /// Why.
        detail: String,
    },
}

impl DbHealth {
    fn to_error(&self, db: DbKind) -> StoreError {
        match self {
            Self::Ok => StoreError::Unavailable {
                db,
                detail: "database is healthy".into(),
            },
            Self::Corrupt { detail } => StoreError::Corrupt {
                db,
                detail: detail.clone(),
            },
            Self::TooNew { found, supported } => StoreError::TooNew {
                db,
                found: *found,
                supported: *supported,
            },
            Self::Unavailable { detail } => StoreError::Unavailable {
                db,
                detail: detail.clone(),
            },
        }
    }
}

struct Conns {
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
}

enum DbState {
    Open(Conns),
    Broken(DbHealth),
}

/// One database file and its connections.
pub(crate) struct Db {
    spec: &'static DbSpec,
    path: PathBuf,
    state: RwLock<DbState>,
    /// Corruption seen at runtime. Kept outside `state` because operations
    /// only hold the read lock when they notice it.
    corrupt: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding a connection leaves it usable: rusqlite rolls the
    // open transaction back on drop during unwinding.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Db {
    /// Opens (creating if needed) the database. Never fails: problems are
    /// recorded and reported by [`Db::health`] and by every operation.
    pub(crate) fn open(spec: &'static DbSpec, path: PathBuf) -> Self {
        let state = open_state(spec, &path);
        Self {
            spec,
            path,
            state: RwLock::new(state),
            corrupt: Mutex::new(None),
        }
    }

    pub(crate) fn kind(&self) -> DbKind {
        self.spec.kind
    }

    pub(crate) fn health(&self) -> DbHealth {
        if let Some(detail) = lock(&self.corrupt).clone() {
            return DbHealth::Corrupt { detail };
        }
        match &*self.read_state() {
            DbState::Open(_) => DbHealth::Ok,
            DbState::Broken(h) => h.clone(),
        }
    }

    fn read_state(&self) -> RwLockReadGuard<'_, DbState> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn usable<'a>(&self, state: &'a DbState) -> Result<&'a Conns> {
        if let Some(detail) = lock(&self.corrupt).clone() {
            return Err(StoreError::Corrupt {
                db: self.kind(),
                detail,
            });
        }
        match state {
            DbState::Open(c) => Ok(c),
            DbState::Broken(h) => Err(h.to_error(self.kind())),
        }
    }

    /// Runs `f` in an `IMMEDIATE` transaction on the writer and commits.
    ///
    /// Any error rolls the whole transaction back.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let state = self.read_state();
        let conns = self.usable(&state)?;
        let mut conn = lock(&conns.writer);
        let result = (|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })();
        result.map_err(|e| self.classify(e))
    }

    /// Runs `f` inside a read transaction on a pooled reader, so it sees one
    /// consistent snapshot even while the writer commits.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let state = self.read_state();
        let conns = self.usable(&state)?;
        let pooled = lock(&conns.readers).pop();
        let conn = match pooled {
            Some(c) => c,
            None => open_reader(&self.path).map_err(|e| self.classify(e))?,
        };
        let result = (|| {
            let tx = conn.unchecked_transaction()?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })();
        let mut readers = lock(&conns.readers);
        if readers.len() < MAX_IDLE_READERS {
            readers.push(conn);
        }
        drop(readers);
        result.map_err(|e| self.classify(e))
    }

    /// Maps corruption error codes to [`StoreError::Corrupt`] and latches the
    /// database as corrupt.
    pub(crate) fn classify(&self, e: StoreError) -> StoreError {
        if let StoreError::Sqlite(inner) = &e
            && is_corruption(inner)
        {
            let detail = inner.to_string();
            *lock(&self.corrupt) = Some(detail.clone());
            return StoreError::Corrupt {
                db: self.kind(),
                detail,
            };
        }
        e
    }

    /// Closes every connection, renames the file (and its `-wal`/`-shm`
    /// sidecars) to `<name>.corrupt-<timestamp>`, and creates a fresh
    /// database. Returns where the old file went, if there was one.
    pub(crate) fn reset(&self, now: Timestamp) -> Result<Option<PathBuf>> {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        *state = DbState::Broken(DbHealth::Unavailable {
            detail: "reset in progress".into(),
        });
        let moved = move_aside(&self.path, now);
        if moved.is_ok() {
            *lock(&self.corrupt) = None;
        }
        *state = open_state(self.spec, &self.path);
        let moved = moved?;
        match &*state {
            DbState::Open(_) => Ok(moved),
            DbState::Broken(h) => Err(h.to_error(self.kind())),
        }
    }
}

fn open_state(spec: &'static DbSpec, path: &Path) -> DbState {
    match open_writer(spec, path) {
        Ok(conn) => DbState::Open(Conns {
            writer: Mutex::new(conn),
            readers: Mutex::new(Vec::new()),
        }),
        Err(e) => DbState::Broken(health_from_open_error(e)),
    }
}

fn health_from_open_error(e: StoreError) -> DbHealth {
    match e {
        StoreError::Corrupt { detail, .. } => DbHealth::Corrupt { detail },
        StoreError::TooNew {
            found, supported, ..
        } => DbHealth::TooNew { found, supported },
        StoreError::Sqlite(inner) if is_corruption(&inner) => DbHealth::Corrupt {
            detail: inner.to_string(),
        },
        other => DbHealth::Unavailable {
            detail: other.to_string(),
        },
    }
}

fn open_writer(spec: &'static DbSpec, path: &Path) -> Result<Connection> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // NOTE: on filesystems without shared memory (some network shares) SQLite
    // silently stays in rollback-journal mode. The store still works; readers
    // just block during writes.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", spec.synchronous)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    quick_check(&conn, spec.kind)?;
    migrate_to(&mut conn, spec.kind, spec.migrations, None)?;
    Ok(conn)
}

fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// `PRAGMA quick_check`: O(pages) but skips index cross-checks, so it stays
/// fast enough to run on every open (tens of ms for a 100 MB file).
fn quick_check(conn: &Connection, db: DbKind) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA quick_check(16)")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() == 1 && rows[0] == "ok" {
        Ok(())
    } else {
        Err(StoreError::Corrupt {
            db,
            detail: rows.join("; "),
        })
    }
}

/// Applies every migration above the file's `user_version`, up to `target`
/// (or the latest when `None`).
pub(crate) fn migrate_to(
    conn: &mut Connection,
    db: DbKind,
    migrations: &[Migration],
    target: Option<i64>,
) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let latest = latest_version(migrations);
    if current > latest {
        return Err(StoreError::TooNew {
            db,
            found: current,
            supported: latest,
        });
    }
    let target = target.unwrap_or(latest);
    for m in migrations
        .iter()
        .filter(|m| m.version > current && m.version <= target)
    {
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql).map_err(|e| {
            if is_corruption(&e) {
                StoreError::Sqlite(e)
            } else {
                StoreError::Unavailable {
                    db,
                    detail: format!("migration {} ({}) failed: {e}", m.version, m.name),
                }
            }
        })?;
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
    }
    Ok(())
}

/// Renames `path` and its sidecars aside. Returns the new main path, or
/// `None` if there was no file.
fn move_aside(path: &Path, now: Timestamp) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = format!("{file_name}.corrupt-{}", now.to_compact_string());
    let mut target = path.with_file_name(&base);
    let mut n = 1;
    while target.exists() {
        target = path.with_file_name(format!("{base}-{n}"));
        n += 1;
    }
    std::fs::rename(path, &target).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    for suffix in ["-wal", "-shm"] {
        let side = sidecar(path, suffix);
        if side.exists() {
            let side_target = sidecar(&target, suffix);
            std::fs::rename(&side, &side_target).map_err(|source| StoreError::Io {
                path: side.clone(),
                source,
            })?;
        }
    }
    Ok(Some(target))
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Normalized DDL of every user object, for schema snapshot tests.
#[cfg(test)]
pub(crate) fn schema_dump(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
         ORDER BY type DESC, name",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .iter()
        .map(|s| format!("{};\n", s.trim()))
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_path(kind: DbKind) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schema")
            .join(format!("{kind}.sql"))
    }

    fn fresh_schema(spec: &'static DbSpec) -> String {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(spec, dir.path().join("x.db"));
        assert_eq!(db.health(), DbHealth::Ok);
        db.read(schema_dump).unwrap()
    }

    fn check_snapshot(spec: &'static DbSpec) {
        let actual = fresh_schema(spec);
        let path = snapshot_path(spec.kind);
        if std::env::var_os("STRATA_UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert_eq!(
            actual,
            expected,
            "schema snapshot {} is stale; rerun with STRATA_UPDATE_SNAPSHOTS=1 and review",
            path.display()
        );
    }

    #[test]
    fn history_schema_matches_snapshot() {
        check_snapshot(&HISTORY_SPEC);
    }

    #[test]
    fn state_schema_matches_snapshot() {
        check_snapshot(&STATE_SPEC);
    }

    fn migrate_from_every_version(spec: &'static DbSpec) {
        let expected = fresh_schema(spec);
        for start in 0..=latest_version(spec.migrations) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("x.db");
            {
                let mut conn = Connection::open(&path).unwrap();
                migrate_to(&mut conn, spec.kind, spec.migrations, Some(start)).unwrap();
                let v: i64 = conn
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(v, start);
            }
            let db = Db::open(spec, path);
            assert_eq!(db.health(), DbHealth::Ok, "from v{start}");
            let schema = db.read(schema_dump).unwrap();
            assert_eq!(schema, expected, "migrating from v{start}");
            let v: i64 = db
                .read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
                .unwrap();
            assert_eq!(v, latest_version(spec.migrations));
        }
    }

    #[test]
    fn history_migrates_from_every_version() {
        migrate_from_every_version(&HISTORY_SPEC);
    }

    #[test]
    fn state_migrates_from_every_version() {
        migrate_from_every_version(&STATE_SPEC);
    }

    #[test]
    fn migration_versions_are_strictly_increasing() {
        for list in [HISTORY_MIGRATIONS, STATE_MIGRATIONS] {
            for (i, m) in list.iter().enumerate() {
                assert_eq!(m.version, i as i64 + 1, "{}", m.name);
            }
        }
    }

    #[test]
    fn newer_schema_is_reported_not_touched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99).unwrap();
        }
        let db = Db::open(&HISTORY_SPEC, path.clone());
        assert_eq!(
            db.health(),
            DbHealth::TooNew {
                found: 99,
                supported: 3
            }
        );
        let err = db.read(|_| Ok(())).unwrap_err();
        assert!(matches!(err, StoreError::TooNew { found: 99, .. }));
        drop(db);
        let conn = Connection::open(&path).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 99);
    }

    #[test]
    fn runtime_corruption_codes_latch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&HISTORY_SPEC, dir.path().join("x.db"));
        let err = db
            .write(|_| {
                Err::<(), _>(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                    Some("database disk image is malformed".into()),
                )))
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }));
        assert!(matches!(db.health(), DbHealth::Corrupt { .. }));
        assert!(matches!(
            db.read(|_| Ok(())).unwrap_err(),
            StoreError::Corrupt { .. }
        ));
        db.reset(Timestamp(0)).unwrap();
        assert_eq!(db.health(), DbHealth::Ok);
    }

    #[test]
    fn wal_and_pragmas_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&STATE_SPEC, dir.path().join("x.db"));
        db.write(|tx| {
            let mode: String = tx.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            let fk: i64 = tx.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            assert_eq!(fk, 1);
            let sync: i64 = tx.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            assert_eq!(sync, 2, "FULL");
            Ok(())
        })
        .unwrap();
    }
}
