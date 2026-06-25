-- Richer per-track metadata + embedded cover art (content-addressed, deduped).

ALTER TABLE tracks ADD COLUMN total_tracks  INTEGER;
ALTER TABLE tracks ADD COLUMN total_discs   INTEGER;
ALTER TABLE tracks ADD COLUMN composer      TEXT;
ALTER TABLE tracks ADD COLUMN comment       TEXT;
ALTER TABLE tracks ADD COLUMN isrc          TEXT;
ALTER TABLE tracks ADD COLUMN label         TEXT;
ALTER TABLE tracks ADD COLUMN bpm           INTEGER;
ALTER TABLE tracks ADD COLUMN compilation   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN lyrics        TEXT;
-- MusicBrainz IDs lifted from Picard-style tags (recording_mbid already exists).
ALTER TABLE tracks ADD COLUMN release_mbid  TEXT;
ALTER TABLE tracks ADD COLUMN mb_artist_id  TEXT;
-- Points at the deduped cover art for this track's album.
ALTER TABLE tracks ADD COLUMN cover_hash    TEXT;

-- Embedded album art, deduped by SHA-256 of the image bytes. One blob shared by every track that
-- carries the same artwork (i.e. a whole album).
CREATE TABLE cover_art (
    hash       TEXT PRIMARY KEY,
    mime       TEXT NOT NULL,
    bytes      BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX tracks_cover_hash_idx ON tracks (cover_hash);
