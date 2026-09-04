-- The bot's look as a persistent job: the avatar (the Chordia mark in the accent, unless the owner
-- uploaded their own), and the rate-limit bookkeeping that lets a change wait for Discord.
ALTER TABLE discord_bot_settings ADD COLUMN avatar_managed        INTEGER NOT NULL DEFAULT 1;
ALTER TABLE discord_bot_settings ADD COLUMN avatar_hex_applied    TEXT;
ALTER TABLE discord_bot_settings ADD COLUMN avatar_custom_path    TEXT;
ALTER TABLE discord_bot_settings ADD COLUMN avatar_custom_applied INTEGER NOT NULL DEFAULT 0;
-- Epoch millis before which no theme request may be sent (set from a 429's retry_after).
ALTER TABLE discord_bot_settings ADD COLUMN theme_retry_at        INTEGER;
ALTER TABLE discord_bot_settings ADD COLUMN theme_warning         TEXT;
-- How many limits in a row; widens the margin added to retry_after.
ALTER TABLE discord_bot_settings ADD COLUMN theme_backoff         INTEGER NOT NULL DEFAULT 0;
