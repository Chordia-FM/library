-- The Hub's canonical primary artist for an album (resolved on the Hub via MusicBrainz), so on-disk
-- organization can file an album under the SAME name the Hub shows: folding "Machine Gun Kelly"/"MGK"
-- into "mgk", and correcting collabs (a "Wiz Khalifa & MGK"-tagged album whose real lead is "mgk").
-- NULL until the Hub reports it; the organizer prefers it over the local album-artist tag when set, and
-- relocates files when it changes.
ALTER TABLE albums ADD COLUMN canonical_album_artist TEXT;
