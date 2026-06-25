-- Bounded loudness retries: count failed analysis attempts so permanently-undecodable files stop
-- reoccupying the batch (and aren't re-decoded by ffmpeg on every idle cycle forever).
ALTER TABLE files ADD COLUMN rg_attempts INTEGER NOT NULL DEFAULT 0;

-- File freshness for incremental rescans: a periodic/startup scan can stat each file and skip the
-- full re-probe + SHA-256 when mtime+size are unchanged (the fs watcher still handles live edits).
ALTER TABLE file_paths ADD COLUMN mtime_ns   INTEGER;
ALTER TABLE file_paths ADD COLUMN size_bytes INTEGER;
