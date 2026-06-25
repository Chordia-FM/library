-- Normalize the library catalog so each entity's data lives in its own table.
--
-- The `artists` and `albums` tables were stubbed in 0001 but never populated — every track row
-- duplicated its artist/album/year/genre/etc. Now tracks reference `artists`/`albums` by FK and the
-- album/artist-level fields live on those tables. Track-specific fields (title, isrc, recording_mbid,
-- acoustid, composer, comment, bpm, lyrics, cover, duration) stay on `tracks`.
--
-- Existing rows are repopulated by the forced re-scan below (clearing file freshness): the Rust
-- indexer upserts artists/albums and links each track. We can't migrate the data in pure SQL because
-- the normalize() used for dedup keys isn't reproducible in SQLite (it strips punctuation), and
-- `album_artist` has no stored normalized form. The files (codec/loudness) + cover_art tables are
-- untouched, so the re-scan is cheap-ish and loses nothing the file tags don't already carry.

-- Album/artist-level fields lifted off tracks.
ALTER TABLE artists ADD COLUMN mbid TEXT;
ALTER TABLE albums  ADD COLUMN title_normalized TEXT;
ALTER TABLE albums  ADD COLUMN genre        TEXT;
ALTER TABLE albums  ADD COLUMN label        TEXT;
ALTER TABLE albums  ADD COLUMN total_tracks INTEGER;
ALTER TABLE albums  ADD COLUMN total_discs  INTEGER;
ALTER TABLE albums  ADD COLUMN compilation  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE albums  ADD COLUMN release_mbid TEXT;
ALTER TABLE albums  ADD COLUMN cover_hash   TEXT;

-- Dedup keys for the indexer's upserts.
CREATE UNIQUE INDEX IF NOT EXISTS artists_name_norm_uniq   ON artists(name_normalized);
CREATE UNIQUE INDEX IF NOT EXISTS albums_title_artist_uniq ON albums(title_normalized, artist_id);

-- Tracks reference artists/albums by FK.
ALTER TABLE tracks ADD COLUMN artist_id TEXT REFERENCES artists(id);
ALTER TABLE tracks ADD COLUMN album_id  TEXT REFERENCES albums(id);

-- Drop the index over columns we're about to remove, then drop the denormalized columns.
DROP INDEX IF EXISTS tracks_fuzzy_idx;
ALTER TABLE tracks DROP COLUMN artist;
ALTER TABLE tracks DROP COLUMN artist_norm;
ALTER TABLE tracks DROP COLUMN album;
ALTER TABLE tracks DROP COLUMN album_norm;
ALTER TABLE tracks DROP COLUMN album_artist;
ALTER TABLE tracks DROP COLUMN year;
ALTER TABLE tracks DROP COLUMN genre;
ALTER TABLE tracks DROP COLUMN total_tracks;
ALTER TABLE tracks DROP COLUMN total_discs;
ALTER TABLE tracks DROP COLUMN label;
ALTER TABLE tracks DROP COLUMN compilation;
ALTER TABLE tracks DROP COLUMN release_mbid;
ALTER TABLE tracks DROP COLUMN mb_artist_id;

-- Own-copy fuzzy match now joins artists for the normalized name; index the track side.
CREATE INDEX tracks_title_norm_idx ON tracks(title_norm, duration_ms);
CREATE INDEX tracks_artist_id_idx  ON tracks(artist_id);
CREATE INDEX tracks_album_id_idx   ON tracks(album_id);

-- Force a full re-scan: the indexer repopulates artists/albums + the FKs on the next startup scan.
UPDATE file_paths SET mtime_ns = NULL, size_bytes = NULL;
