-- Chordia library - local index (SQLite). Ids are app-assigned UUIDv7 stored as TEXT.

CREATE TABLE libraries (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    path       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE artists (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    name_normalized TEXT NOT NULL
);

CREATE TABLE albums (
    id        TEXT PRIMARY KEY,
    title     TEXT NOT NULL,
    artist_id TEXT REFERENCES artists (id),
    year      INTEGER
);

CREATE TABLE tracks (
    id             TEXT PRIMARY KEY,
    library_id     TEXT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    path           TEXT NOT NULL,
    title          TEXT NOT NULL,
    artist         TEXT NOT NULL,
    album          TEXT,
    track_no       INTEGER,
    disc_no        INTEGER,
    -- audio properties
    codec          TEXT NOT NULL,
    sample_rate_hz INTEGER NOT NULL,
    bit_depth      INTEGER NOT NULL,
    channels       INTEGER NOT NULL,
    lossless       INTEGER NOT NULL DEFAULT 0,   -- bool
    spatial        INTEGER NOT NULL DEFAULT 0,   -- bool; passthrough_only
    duration_ms    INTEGER NOT NULL DEFAULT 0,
    -- layered fingerprint (own-copy matching)
    acoustid       TEXT,
    recording_mbid TEXT,
    content_hash   TEXT NOT NULL,
    artist_norm    TEXT NOT NULL,
    title_norm     TEXT NOT NULL,
    album_norm     TEXT,
    UNIQUE (library_id, path)
);

-- Own-copy match indexes (strongest → weakest).
CREATE INDEX tracks_acoustid_idx ON tracks (acoustid);
CREATE INDEX tracks_recording_mbid_idx ON tracks (recording_mbid);
CREATE INDEX tracks_content_hash_idx ON tracks (content_hash);
CREATE INDEX tracks_fuzzy_idx ON tracks (artist_norm, title_norm, duration_ms);

-- Durable offline scrobble buffer (forwarded to the Hub, deduped there on event_id).
CREATE TABLE scrobble_queue (
    event_id   TEXT PRIMARY KEY,             -- UUIDv7, idempotency key
    payload    TEXT NOT NULL,                -- serialized ListeningEvent (JSON)
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    attempts   INTEGER NOT NULL DEFAULT 0,
    flushed    INTEGER NOT NULL DEFAULT 0    -- bool
);
CREATE INDEX scrobble_queue_pending_idx ON scrobble_queue (flushed, created_at);
