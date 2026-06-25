-- Durable buffer of the owner's listening events, forwarded to the Hub by the scrobble reporter.
-- Events are stored as the full `ListeningEvent` JSON so the schema never drifts from the contract.
-- `event_id` (UUIDv7) is the idempotency key: re-enqueues are ignored, and the Hub dedupes on it
-- too, so re-sends after a partial failure never double-count.
CREATE TABLE IF NOT EXISTS pending_scrobbles (
    event_id   TEXT PRIMARY KEY,
    payload    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pending_scrobbles_created ON pending_scrobbles (created_at);
