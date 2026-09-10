-- A session summary when the bot leaves, crossfade between tracks, and the queue a 24/7 bot
-- keeps across a restart.
ALTER TABLE discord_guild_settings ADD COLUMN summary INTEGER NOT NULL DEFAULT 1;
ALTER TABLE discord_guild_settings ADD COLUMN crossfade_secs INTEGER NOT NULL DEFAULT 0;
CREATE TABLE discord_saved_queue (
    app_id   TEXT NOT NULL,
    guild_id TEXT NOT NULL,
    state    TEXT NOT NULL,
    saved_at INTEGER NOT NULL,
    PRIMARY KEY (app_id, guild_id)
);
