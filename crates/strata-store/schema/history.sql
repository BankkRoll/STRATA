CREATE TABLE activity_hourly (
    hour          INTEGER NOT NULL,
    image_id      INTEGER NOT NULL REFERENCES process_images (id) ON DELETE CASCADE,
    dir_hash      INTEGER NOT NULL,
    bytes_written INTEGER NOT NULL DEFAULT 0,
    files_created INTEGER NOT NULL DEFAULT 0,
    files_deleted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (hour, image_id, dir_hash)
) WITHOUT ROWID;

CREATE TABLE hash_cache (
    volume_id INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    file_ref  INTEGER NOT NULL,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    partial   INTEGER NOT NULL,
    full      BLOB CHECK (full IS NULL OR length(full) = 32),
    PRIMARY KEY (volume_id, file_ref)
) WITHOUT ROWID;

CREATE TABLE last_writer (
    path_hash  INTEGER PRIMARY KEY,
    image_id   INTEGER NOT NULL REFERENCES process_images (id) ON DELETE CASCADE,
    pid        INTEGER,
    written_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE paths (
    id          INTEGER PRIMARY KEY,
    hash        INTEGER NOT NULL UNIQUE,
    parent_hash INTEGER,
    path        TEXT    NOT NULL
);

CREATE TABLE process_images (
    id   INTEGER PRIMARY KEY,
    path TEXT    NOT NULL UNIQUE
);

CREATE TABLE snapshot_dirs (
    snapshot_id INTEGER PRIMARY KEY REFERENCES snapshots (id) ON DELETE CASCADE,
    codec       INTEGER NOT NULL,
    data        BLOB    NOT NULL
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

CREATE TABLE volumes (
    id          INTEGER PRIMARY KEY,
    serial      INTEGER NOT NULL,
    guid_path   TEXT    NOT NULL,
    UNIQUE (serial, guid_path)
);

CREATE INDEX activity_by_dir ON activity_hourly (dir_hash, hour);

CREATE INDEX snapshots_by_volume_time ON snapshots (volume_id, taken_at);
