-- Per-library on-disk organisation. When `organize` is on, audio files are laid out from
-- `organize_template` (e.g. `{albumartist}/{album}/{track} - {title}`) under the library root.
-- Off by default so existing libraries are untouched until the owner opts in.
ALTER TABLE libraries ADD COLUMN organize          INTEGER NOT NULL DEFAULT 0;
ALTER TABLE libraries ADD COLUMN organize_template TEXT;
