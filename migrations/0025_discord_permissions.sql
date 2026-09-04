-- Presence per mode: single-server mode keeps presence_template; multi-server mode rotates
-- through a list of statuses on an interval. Both are kept whichever mode is active.
ALTER TABLE discord_bot_settings ADD COLUMN multi_statuses     TEXT    NOT NULL DEFAULT '["/play"]';
ALTER TABLE discord_bot_settings ADD COLUMN status_rotate_secs INTEGER NOT NULL DEFAULT 60;

-- Several DJ roles instead of one; the old column folds into the list.
ALTER TABLE discord_guild_settings ADD COLUMN dj_role_ids TEXT NOT NULL DEFAULT '[]';
UPDATE discord_guild_settings SET dj_role_ids = json_array(dj_role_id)
 WHERE dj_role_id IS NOT NULL AND dj_role_id <> '';
ALTER TABLE discord_guild_settings DROP COLUMN dj_role_id;

-- What the library owner lets a server do. 24/7 and autoplay stay the server's own call (a
-- command, a button) inside these gates.
ALTER TABLE discord_guild_settings ADD COLUMN can_always_on  INTEGER NOT NULL DEFAULT 1;
ALTER TABLE discord_guild_settings ADD COLUMN can_autoplay   INTEGER NOT NULL DEFAULT 1;
-- Messages after the controller before it is re-posted at the bottom of the channel.
ALTER TABLE discord_guild_settings ADD COLUMN announce_after INTEGER NOT NULL DEFAULT 20;
