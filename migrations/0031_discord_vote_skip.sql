-- How a track gets skipped: 'single' (one person allowed to control playback skips at once)
-- or 'vote' (a share of the listeners must ask), and that share in percent.
ALTER TABLE discord_guild_settings ADD COLUMN skip_mode TEXT NOT NULL DEFAULT 'single';
ALTER TABLE discord_guild_settings ADD COLUMN vote_percent INTEGER NOT NULL DEFAULT 50;
