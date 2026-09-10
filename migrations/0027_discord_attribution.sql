-- A queued play may belong to someone in the Discord voice channel rather than the owner. The
-- Discord id is kept rather than a Hub user id: the reporter resolves it when it sends, so an
-- unlinked or opted-out listener drops out at send time and a Hub outage loses nothing.
ALTER TABLE pending_scrobbles ADD COLUMN discord_user_id TEXT;
