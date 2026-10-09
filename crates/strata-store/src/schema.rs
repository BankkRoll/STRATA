//! Embedded, ordered schema migrations for both database files.
//!
//! Each database records its schema version in `PRAGMA user_version`. On
//! open, every migration with a higher version is applied in order, each in
//! its own transaction together with the `user_version` bump, so a crash
//! mid-migration leaves the file at the previous version.
//!
//! Rules for adding a migration:
//! - Append; never edit a shipped migration.
//! - Bump nothing else: the latest version is the last entry's `version`.
//! - Regenerate the schema snapshot (`STRATA_UPDATE_SNAPSHOTS=1 cargo test -p
//!   strata-store schema`) and review the diff.
//!
//! Integers that are conceptually `u64` (hashes, file references, FILETIMEs)
//! are stored bit-cast to `i64`; sizes never approach 2^63 so `SUM` over them
//! stays meaningful.

/// One schema step.
#[derive(Debug)]
pub(crate) struct Migration {
    /// `user_version` after this step.
    pub version: i64,
    /// Short description, shown in errors.
    pub name: &'static str,
    /// DDL executed as one batch.
    pub sql: &'static str,
}

// -----------------------------------------------------------------------------
// history.db
// -----------------------------------------------------------------------------

const HISTORY_V1_SNAPSHOTS: &str = "
CREATE TABLE volumes (
    id          INTEGER PRIMARY KEY,
    serial      INTEGER NOT NULL,
    guid_path   TEXT    NOT NULL,
    UNIQUE (serial, guid_path)
);

CREATE TABLE paths (
    id          INTEGER PRIMARY KEY,
    hash        INTEGER NOT NULL UNIQUE,
    parent_hash INTEGER,
    path        TEXT    NOT NULL
);

CREATE TABLE snapshots (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    taken_at      INTEGER NOT NULL,
    total_bytes   INTEGER NOT NULL,
    free_bytes    INTEGER NOT NULL,
    allocated_sum INTEGER NOT NULL,
    logical_sum   INTEGER NOT NULL,
    file_count    INTEGER NOT NULL,
    dir_count     INTEGER NOT NULL,
    scanner       TEXT    NOT NULL,
    min_dir_bytes INTEGER NOT NULL,
    stored_dirs   INTEGER NOT NULL
);

CREATE INDEX snapshots_by_volume_time ON snapshots (volume_id, taken_at);

CREATE TABLE snapshot_dirs (
    snapshot_id INTEGER PRIMARY KEY REFERENCES snapshots (id) ON DELETE CASCADE,
    codec       INTEGER NOT NULL,
    data        BLOB    NOT NULL
);
";

const HISTORY_V2_ACTIVITY: &str = "
CREATE TABLE process_images (
    id   INTEGER PRIMARY KEY,
    path TEXT    NOT NULL UNIQUE
);

CREATE TABLE activity_hourly (
    hour          INTEGER NOT NULL,
    image_id      INTEGER NOT NULL REFERENCES process_images (id) ON DELETE CASCADE,
    dir_hash      INTEGER NOT NULL,
    bytes_written INTEGER NOT NULL DEFAULT 0,
    files_created INTEGER NOT NULL DEFAULT 0,
    files_deleted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (hour, image_id, dir_hash)
) WITHOUT ROWID;

CREATE INDEX activity_by_dir ON activity_hourly (dir_hash, hour);

CREATE TABLE last_writer (
    path_hash  INTEGER PRIMARY KEY,
    image_id   INTEGER NOT NULL REFERENCES process_images (id) ON DELETE CASCADE,
    pid        INTEGER,
    written_at INTEGER NOT NULL
) WITHOUT ROWID;
";

const HISTORY_V3_HASH_CACHE: &str = "
CREATE TABLE hash_cache (
    volume_id INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    file_ref  INTEGER NOT NULL,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    partial   INTEGER NOT NULL,
    full      BLOB CHECK (full IS NULL OR length(full) = 32),
    PRIMARY KEY (volume_id, file_ref)
) WITHOUT ROWID;
";

/// Migrations for `history.db`.
pub(crate) const HISTORY_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "snapshots",
        sql: HISTORY_V1_SNAPSHOTS,
    },
    Migration {
        version: 2,
        name: "activity rollups",
        sql: HISTORY_V2_ACTIVITY,
    },
    Migration {
        version: 3,
        name: "duplicate hash cache",
        sql: HISTORY_V3_HASH_CACHE,
    },
];

// -----------------------------------------------------------------------------
// state.db
// -----------------------------------------------------------------------------

const STATE_V1_SETTINGS: &str = "
CREATE TABLE settings (
    key        TEXT    PRIMARY KEY,
    version    INTEGER NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;
";

const STATE_V2_UNDO_LOG: &str = "
CREATE TABLE actions (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,
    status      TEXT    NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE INDEX actions_in_progress ON actions (id) WHERE status = 'in_progress';

CREATE TABLE action_items (
    id            INTEGER PRIMARY KEY,
    action_id     INTEGER NOT NULL REFERENCES actions (id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,
    path          TEXT    NOT NULL,
    volume_serial INTEGER NOT NULL,
    volume_guid   TEXT    NOT NULL,
    file_ref      INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    mtime         INTEGER NOT NULL,
    method        TEXT    NOT NULL,
    tier          TEXT    NOT NULL,
    rule_id       TEXT,
    result        TEXT    NOT NULL DEFAULT 'pending',
    error         TEXT,
    completed_at  INTEGER,
    original_path TEXT,
    restore_blob  BLOB,
    restored_at   INTEGER,
    UNIQUE (action_id, seq)
);

CREATE INDEX action_items_restorable ON action_items (completed_at)
    WHERE method = 'recycle' AND result = 'done' AND restored_at IS NULL;
";

const STATE_V3_LICENSE: &str = "
CREATE TABLE license (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    payload      BLOB    NOT NULL,
    signature    BLOB    NOT NULL,
    activated_at INTEGER NOT NULL
);
";

/// Migrations for `state.db`.
pub(crate) const STATE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "settings",
        sql: STATE_V1_SETTINGS,
    },
    Migration {
        version: 2,
        name: "undo and audit log",
        sql: STATE_V2_UNDO_LOG,
    },
    Migration {
        version: 3,
        name: "license",
        sql: STATE_V3_LICENSE,
    },
];

/// Latest version in a migration list (0 for an empty list).
pub(crate) fn latest_version(migrations: &[Migration]) -> i64 {
    migrations.last().map_or(0, |m| m.version)
}
