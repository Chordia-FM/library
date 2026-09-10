-- How the bot lays out its messages, as JSON (chordia_contracts::discord_layout::BotLayouts).
-- NULL is the shipped design.
ALTER TABLE discord_bot_settings ADD COLUMN layouts TEXT;
