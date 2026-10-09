//! Typed errors for every store operation.
//!
//! Corruption is surfaced as [`StoreError::Corrupt`] no matter where SQLite
//! notices it (the open-time `quick_check`, or `SQLITE_CORRUPT` /
//! `SQLITE_NOTADB` from any later statement), so the app has exactly one case
//! to handle with "reset history".

use std::fmt;
use std::path::PathBuf;

use crate::settings::SettingsIssue;

/// Which database file an error or health report refers to.
///
/// History and state live in separate files so resetting a corrupt history
/// never touches settings, the undo log or the license.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    /// `history.db`: snapshots, activity rollups, the duplicate-hash cache.
    /// Everything in it is derived data that can be rebuilt by rescanning.
    History,
    /// `state.db`: settings, the undo/audit log and the license.
    State,
}

impl DbKind {
    /// File name of this database inside the store directory.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::History => "history.db",
            Self::State => "state.db",
        }
    }
}

impl fmt::Display for DbKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::History => "history",
            Self::State => "state",
        })
    }
}

/// Error returned by store operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The database file is damaged or is not a SQLite database. Call
    /// [`Store::reset_history`](crate::Store::reset_history) (or
    /// [`Store::reset_state`](crate::Store::reset_state)) to move it aside.
    #[error("{db} database is corrupt: {detail}")]
    Corrupt {
        /// Affected database.
        db: DbKind,
        /// SQLite's description of the damage.
        detail: String,
    },
    /// The file was written by a newer Strata with a schema this build does
    /// not understand. The file is left untouched.
    #[error("{db} database schema version {found} is newer than supported version {supported}")]
    TooNew {
        /// Affected database.
        db: DbKind,
        /// `user_version` found in the file.
        found: i64,
        /// Latest version this build knows.
        supported: i64,
    },
    /// The database could not be opened (locked by another process,
    /// permissions, disk full while creating it).
    #[error("{db} database is unavailable: {detail}")]
    Unavailable {
        /// Affected database.
        db: DbKind,
        /// Why it could not be opened.
        detail: String,
    },
    /// A filesystem operation outside SQLite failed.
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        /// Path being operated on.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Any other SQLite error.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The referenced row does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// The caller passed an argument the store rejects.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Settings failed validation; nothing was written.
    #[error("invalid settings: {}", join_issues(.0))]
    InvalidSettings(Vec<SettingsIssue>),
    /// JSON encoding or decoding failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

fn join_issues(issues: &[SettingsIssue]) -> String {
    issues
        .iter()
        .map(|i| format!("{}: {}", i.key, i.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Result alias used throughout the crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Whether a SQLite error means the file itself is damaged.
pub(crate) fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}
