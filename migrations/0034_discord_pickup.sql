-- Where the bot picks up after a restart: where it left off, or the start of the track.
ALTER TABLE discord_guild_settings ADD COLUMN pickup TEXT NOT NULL DEFAULT 'position';
