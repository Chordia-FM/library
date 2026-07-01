-- Acquisition resume bookkeeping: track in-flight downloads so a library restart re-attaches the
-- monitor (re-polls qBittorrent and imports on completion) instead of orphaning a grab.
CREATE TABLE acquisition_jobs (
    job_id          TEXT PRIMARY KEY,   -- the Hub download_jobs.id this grab is for
    qbit_hash       TEXT NOT NULL,
    hub_library_id  TEXT NOT NULL,      -- the Hub library id, mapped to a local path on resume
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
