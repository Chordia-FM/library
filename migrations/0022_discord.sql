-- Discord music bot: per-identity settings, per-guild settings, and a local play log.
--
-- Tokens are NOT here — they stay in the TOML config. These rows are everything about a bot that is
-- worth changing at runtime from the dashboard or from Discord itself. Keyed by the Discord
-- application id so a rotated token keeps its settings.

CREATE TABLE discord_bot_settings (
    app_id            TEXT PRIMARY KEY,
    -- Overrides the bot user's own name in views and in the dashboard. NULL = use the bot's name.
    display_name      TEXT,
    -- 'multi': the bot serves any guild and shows a static presence. 'single': one guild is the
    -- point, so the presence and voice-channel status follow that guild's now-playing.
    mode              TEXT NOT NULL DEFAULT 'multi' CHECK (mode IN ('multi', 'single')),
    presence_template TEXT NOT NULL DEFAULT '{title} — {artist}',
    default_volume    INTEGER NOT NULL DEFAULT 100,
    idle_timeout_secs INTEGER NOT NULL DEFAULT 300,
    -- JSON array of guild ids the bot may serve; NULL = any guild it is invited to.
    allowed_guilds    TEXT,
    -- JSON array of Discord user ids treated as the owner (DJ everywhere, admin everywhere).
    owner_discord_ids TEXT NOT NULL DEFAULT '[]',
    -- Set the voice channel's status line to the now-playing track.
    vc_status         INTEGER NOT NULL DEFAULT 1,
    -- Hash of the slash-command set last registered with Discord, so boot only re-registers when
    -- the commands actually changed (registration is rate limited and global commands propagate
    -- slowly).
    commands_hash     TEXT,
    updated_at        INTEGER NOT NULL
);

CREATE TABLE discord_guild_settings (
    app_id                TEXT NOT NULL,
    guild_id              TEXT NOT NULL,
    dj_role_id            TEXT,
    -- Where the now-playing controller lives, so it can be edited/deleted across restarts.
    controller_channel_id TEXT,
    controller_message_id TEXT,
    -- NULL = the bot's default_volume.
    volume                INTEGER,
    normalize             INTEGER NOT NULL DEFAULT 1,
    -- 24/7: never idle-leave, and rejoin always_on_channel_id at boot.
    always_on             INTEGER NOT NULL DEFAULT 0,
    always_on_channel_id  TEXT,
    autoplay              INTEGER NOT NULL DEFAULT 0,
    -- Re-post the controller at the bottom of the channel when it has scrolled away.
    announce              INTEGER NOT NULL DEFAULT 1,
    updated_at            INTEGER NOT NULL,
    PRIMARY KEY (app_id, guild_id)
);

-- Every track the bot played, for /stats. Local to this host; never forwarded anywhere.
CREATE TABLE discord_plays (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id       TEXT NOT NULL,
    guild_id     TEXT NOT NULL,
    track_id     TEXT NOT NULL,
    requested_by TEXT,
    started_at   INTEGER NOT NULL,
    ms_played    INTEGER NOT NULL DEFAULT 0,
    listeners    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX discord_plays_guild_idx ON discord_plays (app_id, guild_id, started_at);
CREATE INDEX discord_plays_track_idx ON discord_plays (track_id);
