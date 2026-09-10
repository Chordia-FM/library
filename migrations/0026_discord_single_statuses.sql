-- Single-server mode rotates through statuses too; the one template folds into the list.
ALTER TABLE discord_bot_settings ADD COLUMN single_statuses TEXT NOT NULL DEFAULT '["{title} · {artist}"]';
UPDATE discord_bot_settings SET single_statuses = json_array(presence_template)
 WHERE presence_template IS NOT NULL AND presence_template <> '';
ALTER TABLE discord_bot_settings DROP COLUMN presence_template;
