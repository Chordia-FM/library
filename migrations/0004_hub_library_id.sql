-- Link local library rows to their Hub-side UUID so the streaming handler can
-- cross-check the capability token's library_id claim against the local library.
-- SQLite does not allow ADD COLUMN with UNIQUE; use a partial index instead.
ALTER TABLE libraries ADD COLUMN hub_library_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_libraries_hub_library_id
    ON libraries (hub_library_id) WHERE hub_library_id IS NOT NULL;
