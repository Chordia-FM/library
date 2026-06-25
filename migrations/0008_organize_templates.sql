-- Per-content-type organise templates + de-duplication.
-- `organize_template` stays the album/default template; these add optional templates for tracks
-- with no album (singles) and tracks we couldn't identify (no artist tag). When `dedupe` is on,
-- files that map to the same destination collapse to the single highest-quality copy.
ALTER TABLE libraries ADD COLUMN organize_template_single  TEXT;
ALTER TABLE libraries ADD COLUMN organize_template_unknown TEXT;
ALTER TABLE libraries ADD COLUMN dedupe INTEGER NOT NULL DEFAULT 0;
