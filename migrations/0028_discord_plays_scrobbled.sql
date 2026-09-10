-- How many listeners a play was queued to count for, so /history can say whether it was.
ALTER TABLE discord_plays ADD COLUMN scrobbled_for INTEGER NOT NULL DEFAULT 0;
