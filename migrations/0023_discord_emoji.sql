-- The bot's icon colour: what the dashboard asked for, and what was last uploaded to Discord as
-- application emojis (so a restart never re-uploads an unchanged set). NULL = the default pink.
ALTER TABLE discord_bot_settings ADD COLUMN emoji_hex TEXT;
ALTER TABLE discord_bot_settings ADD COLUMN emoji_hex_applied TEXT;

-- The default presence template used an em dash; rows saved with that default get the new one (a
-- middle dot). A template someone edited by hand is left alone.
UPDATE discord_bot_settings
SET presence_template = '{title} · {artist}'
WHERE presence_template = '{title} — {artist}';
