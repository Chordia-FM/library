-- The loudness worker's recurring work query (`WHERE rg_gain_db IS NULL AND rg_attempts < ?
-- ORDER BY rg_attempts`) otherwise full-scans `files` every pass, forever, on fully-analyzed
-- libraries. Partial index keeps it tiny: only unanalyzed rows are indexed.
CREATE INDEX IF NOT EXISTS idx_files_loudness_pending
    ON files (rg_attempts)
    WHERE rg_gain_db IS NULL;
