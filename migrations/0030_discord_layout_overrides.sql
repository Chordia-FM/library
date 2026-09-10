-- A server's own versions of some of the bot's messages, as JSON
-- (chordia_contracts::discord_layout::LayoutOverrides). NULL is "the bot's".
ALTER TABLE discord_guild_settings ADD COLUMN layout_overrides TEXT;
