-- One-time marker for the deny-by-default allow list (M-34). A bot that upgraded into the new
-- rule has `allowed_guilds` NULL, which now denies every server; on its first boot the bot seeds
-- the list from the servers it is already in and stamps this column. Set means never seed again,
-- so a list the owner later empties stays empty instead of being refilled.
ALTER TABLE discord_bot_settings ADD COLUMN guilds_seeded_at INTEGER;
