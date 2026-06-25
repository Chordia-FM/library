-- Add rich metadata fields that were missing from the initial schema.
-- ALTER TABLE works on SQLite even for columns with DEFAULT NULL.
ALTER TABLE tracks ADD COLUMN year         INTEGER;
ALTER TABLE tracks ADD COLUMN genre        TEXT;
ALTER TABLE tracks ADD COLUMN album_artist TEXT;
