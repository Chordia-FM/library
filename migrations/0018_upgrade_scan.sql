-- Quality-upgrade sweep bookkeeping. `upgrade_scan` paces the per-library sweep interval;
-- `upgrade_attempts` is the ROTATION stamp: an album proposed once isn't proposed again until the
-- retry cooldown passes, so each sweep advances to the next-worst albums instead of re-trying the
-- same ones every week.
CREATE TABLE upgrade_scan (
    library_id  TEXT PRIMARY KEY,
    last_run_ms INTEGER NOT NULL
);

CREATE TABLE upgrade_attempts (
    album_id         TEXT PRIMARY KEY,
    last_proposed_ms INTEGER NOT NULL
);
