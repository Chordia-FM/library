-- The server's equalizer for the bot, as JSON (chordia_contracts::user::EqConfig). NULL is flat
-- and off.
ALTER TABLE discord_guild_settings ADD COLUMN eq TEXT;
