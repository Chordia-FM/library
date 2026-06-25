-- Reorganise track storage into a content-addressed model.
-- Each unique file (identified by SHA-256 of raw bytes) is stored once regardless of how many
-- library folders it appears in.  Libraries become logical membership sets via library_tracks.

-- Physical audio content, keyed by SHA-256 hash.
CREATE TABLE files (
    content_hash   TEXT PRIMARY KEY,
    codec          TEXT NOT NULL,
    sample_rate_hz INTEGER NOT NULL,
    bit_depth      INTEGER NOT NULL,
    channels       INTEGER NOT NULL,
    lossless       INTEGER NOT NULL DEFAULT 0,
    spatial        INTEGER NOT NULL DEFAULT 0,
    duration_ms    INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Known filesystem locations that contain a given content hash.
-- The same song duplicated across two library folders gets two rows here but ONE track row.
CREATE TABLE file_paths (
    id           TEXT PRIMARY KEY,   -- UUIDv7
    content_hash TEXT NOT NULL REFERENCES files(content_hash) ON DELETE CASCADE,
    library_id   TEXT NOT NULL REFERENCES libraries(id)       ON DELETE CASCADE,
    path         TEXT NOT NULL,
    UNIQUE(path)
);
CREATE INDEX file_paths_hash_idx    ON file_paths(content_hash);
CREATE INDEX file_paths_library_idx ON file_paths(library_id);

-- Track metadata, one row per unique content_hash.  Audio properties live in `files`.
-- Replaces the old `tracks` table (no library_id or path; those moved to file_paths/library_tracks).
DROP TABLE IF EXISTS tracks;

CREATE TABLE tracks (
    id              TEXT PRIMARY KEY,
    content_hash    TEXT NOT NULL UNIQUE REFERENCES files(content_hash) ON DELETE CASCADE,
    title           TEXT NOT NULL,
    artist          TEXT NOT NULL,
    album           TEXT,
    track_no        INTEGER,
    disc_no         INTEGER,
    recording_mbid  TEXT,
    acoustid        TEXT,
    artist_norm     TEXT NOT NULL,
    title_norm      TEXT NOT NULL,
    album_norm      TEXT,
    duration_ms     INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX tracks_content_hash_idx ON tracks(content_hash);
CREATE INDEX tracks_acoustid_idx     ON tracks(acoustid);
CREATE INDEX tracks_mbid_idx         ON tracks(recording_mbid);
CREATE INDEX tracks_fuzzy_idx        ON tracks(artist_norm, title_norm, duration_ms);

-- Library membership: which tracks are accessible through which library.
CREATE TABLE library_tracks (
    library_id TEXT NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    track_id   TEXT NOT NULL REFERENCES tracks(id)    ON DELETE CASCADE,
    PRIMARY KEY (library_id, track_id)
);
