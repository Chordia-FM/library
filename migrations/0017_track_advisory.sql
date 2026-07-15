-- Per-track content advisory from the file's iTunes/ID3 rating tag (lofty ParentalAdvisory / raw
-- ITUNESADVISORY), synced to the Hub like `edition` (0014). NULL = unrated; 'explicit' / 'clean'.
-- Tag-derived, overwritten plainly on re-index (not worker-owned, so no COALESCE-preserve).
ALTER TABLE tracks ADD COLUMN advisory TEXT;
