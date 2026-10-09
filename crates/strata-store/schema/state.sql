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

CREATE TABLE actions (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,
    status      TEXT    NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE TABLE license (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    payload      BLOB    NOT NULL,
    signature    BLOB    NOT NULL,
    activated_at INTEGER NOT NULL
);

CREATE TABLE settings (
    key        TEXT    PRIMARY KEY,
    version    INTEGER NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX action_items_restorable ON action_items (completed_at)
    WHERE method = 'recycle' AND result = 'done' AND restored_at IS NULL;

CREATE INDEX actions_in_progress ON actions (id) WHERE status = 'in_progress';
