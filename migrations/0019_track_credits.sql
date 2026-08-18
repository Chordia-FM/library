-- Per-track credits, from the `ALBUM - Tracklist.txt` sidecar a downloader leaves beside an album.
--
-- Tags carry no personnel at all, so this is the only place the library learns who mixed, mastered,
-- produced or published a track. Modelled as (track, person, role) triples rather than a role column
-- on a person, because one name routinely holds several roles on the same track -- "Ant, Producer,
-- Beats, MainArtist" is one person with three, and a single-role column would force either three
-- rows for him or an arbitrary pick.
CREATE TABLE track_credits (
    track_id   TEXT    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    -- As printed. Display uses this; matching uses name_norm.
    name       TEXT    NOT NULL,
    -- Lowercased and whitespace-collapsed, so "Joe  LaPorta" and "joe laporta" are one person
    -- across the album rather than two rows in the credits panel.
    name_norm  TEXT    NOT NULL,
    role       TEXT    NOT NULL,
    -- Publishers arrive in the same list as performers and are only distinguishable by their role.
    -- Flagged at write time so every reader does not have to know that, and so they can be kept out
    -- of artist lists and search where they would otherwise appear to be people.
    is_org     INTEGER NOT NULL DEFAULT 0,
    -- Position in the file, so the panel can render credits in the order the source listed them
    -- rather than alphabetically -- the source order is meaningful (main artist first, publishers
    -- last) and alphabetising it loses that.
    ord        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, name_norm, role)
);

CREATE INDEX track_credits_track_idx ON track_credits (track_id);
-- "everything this person is credited on", which is what makes a credit clickable.
CREATE INDEX track_credits_name_idx ON track_credits (name_norm);

-- When the sidecar was last read into this track, and the file it came from.
--
-- Worker-owned, like every other enrichment stamp here: a re-index must COALESCE-preserve these
-- rather than clearing them, or a rescan silently drops every credit. The hash is what makes a
-- re-read cheap and correct -- unchanged file, nothing to do; edited file, the stamp is stale and
-- the credits are rewritten.
ALTER TABLE tracks ADD COLUMN credits_source_hash TEXT;
ALTER TABLE tracks ADD COLUMN credits_at INTEGER;
