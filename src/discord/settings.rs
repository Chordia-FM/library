//! Runtime settings for a bot identity and for each guild it serves, persisted in SQLite
//! (migration 0022). The token is the only thing about a bot that lives in the TOML file;
//! everything here is meant to be changed while the bot runs, from the dashboard or from Discord.

use chordia_contracts::discord_layout::{BotLayouts, LayoutOverrides};
use chordia_contracts::user::EqConfig;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, SqlitePool};

use crate::discord::eq;
use crate::discord::ui::template;
use crate::error::AppResult;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How a bot presents itself across guilds. The mode only decides what the presence says (and,
/// in single-server mode, whether the voice channel's status follows the track); which servers
/// the bot serves is [`BotSettings::allowed_guilds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BotMode {
    /// Serves any guild; the presence rotates through [`BotSettings::multi_statuses`].
    #[default]
    Multi,
    /// One guild is the point. Presence follows that guild's now-playing, rendered through
    /// [`BotSettings::single_statuses`].
    Single,
}

/// What a single-server bot says until its owner writes their own statuses.
pub const SINGLE_DEFAULT: &str = "{title} · {artist}";
/// What a multi-server bot says until its owner writes their own statuses.
pub const MULTI_DEFAULT: &str = "/play";

/// The most statuses a multi-server bot rotates through.
pub const MAX_STATUSES: usize = 20;
/// Discord caps an activity name at 128 characters.
pub const STATUS_MAX_CHARS: usize = 128;
/// Presence updates are rate limited (5 per 20 s per shard); rotating faster is pointless.
pub const MIN_ROTATE_SECS: u32 = 15;

impl BotMode {
    fn as_str(self) -> &'static str {
        match self {
            BotMode::Multi => "multi",
            BotMode::Single => "single",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "single" => BotMode::Single,
            _ => BotMode::Multi,
        }
    }
}

/// Per-identity settings. Keyed by the Discord application id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotSettings {
    pub app_id: String,
    pub display_name: Option<String>,
    pub mode: BotMode,
    /// Single-server mode: statuses shown in turn while a track plays, every
    /// `status_rotate_secs`. Variables: `{title} {artist} {album} {guild} {channel} {listeners}`.
    pub single_statuses: Vec<String>,
    /// Multi-server mode: statuses shown in turn, every `status_rotate_secs`. Variables:
    /// `{servers} {playing} {listeners} {tracks} {bot}`.
    pub multi_statuses: Vec<String>,
    pub status_rotate_secs: u32,
    pub default_volume: u8,
    pub idle_timeout_secs: u32,
    /// The servers this bot serves. `None` (or empty) = none of them: an invite alone never
    /// reaches the owner's library, the owner allows a server in the dashboard.
    pub allowed_guilds: Option<Vec<String>>,
    pub owner_discord_ids: Vec<String>,
    pub vc_status: bool,
    /// How each of the bot's messages is laid out; the shipped design until edited.
    pub layouts: BotLayouts,
    /// Icon colour for the application emoji set, `#rrggbb`. `None` = the default pink.
    pub emoji_hex: Option<String>,
    /// The colour the set on Discord was last generated in; differs from `emoji_hex` until the
    /// next (re)generation.
    pub emoji_hex_applied: Option<String>,
    /// The avatar is the Chordia mark in the accent, kept in step with `emoji_hex`. Off once the
    /// owner uploads their own image; nothing touches the avatar again until it is turned back on.
    pub avatar_managed: bool,
    pub avatar_hex_applied: Option<String>,
    /// The owner's own avatar file (under `data_dir/discord/`), and whether Discord has it yet.
    pub avatar_custom_path: Option<String>,
    pub avatar_custom_applied: bool,
    /// Epoch millis before which no theme request may go out (from a 429's retry_after).
    pub theme_retry_at: Option<i64>,
    /// What the dashboard shows while a change is waiting or was refused.
    pub theme_warning: Option<String>,
    /// Consecutive rate limits; widens the margin added to the next retry.
    pub theme_backoff: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands_hash: Option<String>,
}

impl BotSettings {
    pub fn defaults(app_id: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            display_name: None,
            mode: BotMode::Multi,
            single_statuses: vec![SINGLE_DEFAULT.to_string()],
            multi_statuses: vec![MULTI_DEFAULT.to_string()],
            status_rotate_secs: 60,
            default_volume: 100,
            idle_timeout_secs: 300,
            allowed_guilds: None,
            owner_discord_ids: Vec::new(),
            vc_status: true,
            layouts: BotLayouts::default(),
            emoji_hex: None,
            emoji_hex_applied: None,
            avatar_managed: true,
            avatar_hex_applied: None,
            avatar_custom_path: None,
            avatar_custom_applied: false,
            theme_retry_at: None,
            theme_warning: None,
            theme_backoff: 0,
            commands_hash: None,
        }
    }

    pub fn is_owner(&self, user_id: u64) -> bool {
        let id = user_id.to_string();
        self.owner_discord_ids.contains(&id)
    }

    /// May the bot serve this server? Only if the owner put it on the list. The list starts empty
    /// and an empty list denies: anyone who reads a bot's application id off its profile can
    /// invite it, and until 2026-09 that was enough to play the owner's library in their own
    /// server. The bot still joins — that is how the server shows up in the dashboard for the
    /// owner to allow — it just answers nothing there.
    pub fn allows_guild(&self, guild_id: u64) -> bool {
        let id = guild_id.to_string();
        self.allowed_guilds
            .as_ref()
            .is_some_and(|list| list.contains(&id))
    }
}

/// A partial update from the dashboard: every field optional, absent means unchanged.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BotSettingsPatch {
    #[serde(default, deserialize_with = "double_option")]
    pub display_name: Option<Option<String>>,
    pub mode: Option<BotMode>,
    pub single_statuses: Option<Vec<String>>,
    pub multi_statuses: Option<Vec<String>>,
    pub status_rotate_secs: Option<u32>,
    pub default_volume: Option<u8>,
    pub idle_timeout_secs: Option<u32>,
    #[serde(default, deserialize_with = "double_option")]
    pub allowed_guilds: Option<Option<Vec<String>>>,
    pub owner_discord_ids: Option<Vec<String>>,
    pub vc_status: Option<bool>,
    /// Checked against the layout rules by the API before it gets here.
    pub layouts: Option<BotLayouts>,
    #[serde(default, deserialize_with = "double_option")]
    pub emoji_hex: Option<Option<String>>,
    pub avatar_managed: Option<bool>,
}

/// `null` clears, absent leaves alone: the standard serde trick for a nullable patch field.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

impl BotSettingsPatch {
    pub fn apply(self, s: &mut BotSettings) {
        if let Some(v) = self.display_name {
            s.display_name = v.filter(|n| !n.trim().is_empty());
        }
        if let Some(v) = self.mode {
            s.mode = v;
        }
        if let Some(v) = self.single_statuses {
            s.single_statuses = clean_statuses(v, SINGLE_DEFAULT);
        }
        if let Some(v) = self.multi_statuses {
            s.multi_statuses = clean_statuses(v, MULTI_DEFAULT);
        }
        if let Some(v) = self.status_rotate_secs {
            s.status_rotate_secs = v.max(MIN_ROTATE_SECS);
        }
        if let Some(v) = self.default_volume {
            s.default_volume = v.min(150);
        }
        if let Some(v) = self.idle_timeout_secs {
            s.idle_timeout_secs = v;
        }
        if let Some(v) = self.allowed_guilds {
            s.allowed_guilds = v.filter(|l| !l.is_empty());
        }
        if let Some(v) = self.owner_discord_ids {
            s.owner_discord_ids = v;
        }
        if let Some(v) = self.vc_status {
            s.vc_status = v;
        }
        if let Some(v) = self.layouts {
            s.layouts = v;
        }
        if let Some(v) = self.emoji_hex {
            s.emoji_hex = v.and_then(|h| crate::discord::emoji::normalize_hex(&h));
        }
        if let Some(v) = self.avatar_managed {
            if v && !s.avatar_managed {
                // Back to the mark: forget what was applied so the job re-uploads it.
                s.avatar_hex_applied = None;
            }
            s.avatar_managed = v;
        }
    }
}

/// Trimmed, capped in length and count, never empty: a bot with nothing to say says the default.
fn clean_statuses(list: Vec<String>, default: &str) -> Vec<String> {
    let list: Vec<String> = list
        .into_iter()
        .map(|t| t.trim().chars().take(STATUS_MAX_CHARS).collect::<String>())
        .filter(|t| !t.is_empty())
        .take(MAX_STATUSES)
        .collect();
    if list.is_empty() {
        vec![default.to_string()]
    } else {
        list
    }
}

fn parse_statuses(json: &str, default: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json)
        .ok()
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| vec![default.to_string()])
}

#[derive(sqlx::FromRow)]
struct BotRow {
    app_id: String,
    display_name: Option<String>,
    mode: String,
    single_statuses: String,
    multi_statuses: String,
    status_rotate_secs: i64,
    default_volume: i64,
    idle_timeout_secs: i64,
    allowed_guilds: Option<String>,
    owner_discord_ids: String,
    vc_status: i64,
    layouts: Option<String>,
    emoji_hex: Option<String>,
    emoji_hex_applied: Option<String>,
    avatar_managed: i64,
    avatar_hex_applied: Option<String>,
    avatar_custom_path: Option<String>,
    avatar_custom_applied: i64,
    theme_retry_at: Option<i64>,
    theme_warning: Option<String>,
    theme_backoff: i64,
    commands_hash: Option<String>,
}

pub async fn load_bot(db: &SqlitePool, app_id: &str) -> AppResult<BotSettings> {
    let row = sqlx::query_as::<_, BotRow>(
        "SELECT app_id, display_name, mode, single_statuses, multi_statuses, \
                status_rotate_secs, default_volume, \
                idle_timeout_secs, allowed_guilds, owner_discord_ids, vc_status, layouts, emoji_hex, \
                emoji_hex_applied, avatar_managed, avatar_hex_applied, avatar_custom_path, \
                avatar_custom_applied, theme_retry_at, theme_warning, theme_backoff, commands_hash \
         FROM discord_bot_settings WHERE app_id = ?",
    )
    .bind(app_id)
    .fetch_optional(db)
    .await?;
    Ok(match row {
        None => BotSettings::defaults(app_id),
        Some(r) => BotSettings {
            app_id: r.app_id,
            display_name: r.display_name,
            mode: BotMode::parse(&r.mode),
            single_statuses: parse_statuses(&r.single_statuses, SINGLE_DEFAULT),
            multi_statuses: parse_statuses(&r.multi_statuses, MULTI_DEFAULT),
            status_rotate_secs: (r.status_rotate_secs.max(0) as u32).max(MIN_ROTATE_SECS),
            default_volume: r.default_volume.clamp(0, 150) as u8,
            idle_timeout_secs: r.idle_timeout_secs.max(0) as u32,
            allowed_guilds: r.allowed_guilds.and_then(|j| serde_json::from_str(&j).ok()),
            owner_discord_ids: serde_json::from_str(&r.owner_discord_ids).unwrap_or_default(),
            vc_status: r.vc_status != 0,
            layouts: r
                .layouts
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .map(|mut l: BotLayouts| {
                    template::upgrade(&mut l);
                    l
                })
                .unwrap_or_default(),
            emoji_hex: r.emoji_hex,
            emoji_hex_applied: r.emoji_hex_applied,
            avatar_managed: r.avatar_managed != 0,
            avatar_hex_applied: r.avatar_hex_applied,
            avatar_custom_path: r.avatar_custom_path,
            avatar_custom_applied: r.avatar_custom_applied != 0,
            theme_retry_at: r.theme_retry_at,
            theme_warning: r.theme_warning,
            theme_backoff: r.theme_backoff.max(0) as u32,
            commands_hash: r.commands_hash,
        },
    })
}

pub async fn save_bot(db: &SqlitePool, s: &BotSettings) -> AppResult<()> {
    let allowed = s
        .allowed_guilds
        .as_ref()
        .map(|l| serde_json::to_string(l).unwrap_or_else(|_| "[]".into()));
    let owners = serde_json::to_string(&s.owner_discord_ids).unwrap_or_else(|_| "[]".into());
    let single = serde_json::to_string(&s.single_statuses).unwrap_or_else(|_| "[]".into());
    let statuses = serde_json::to_string(&s.multi_statuses).unwrap_or_else(|_| "[]".into());
    let layouts = serde_json::to_string(&s.layouts).ok();
    sqlx::query(
        "INSERT INTO discord_bot_settings (app_id, display_name, mode, single_statuses, \
             multi_statuses, status_rotate_secs, \
             default_volume, idle_timeout_secs, allowed_guilds, owner_discord_ids, vc_status, \
             layouts, \
             emoji_hex, emoji_hex_applied, avatar_managed, avatar_hex_applied, \
             avatar_custom_path, avatar_custom_applied, theme_retry_at, theme_warning, \
             theme_backoff, commands_hash, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(app_id) DO UPDATE SET \
             display_name = excluded.display_name, mode = excluded.mode, \
             single_statuses = excluded.single_statuses, \
             multi_statuses = excluded.multi_statuses, \
             status_rotate_secs = excluded.status_rotate_secs, \
             default_volume = excluded.default_volume, \
             idle_timeout_secs = excluded.idle_timeout_secs, \
             allowed_guilds = excluded.allowed_guilds, \
             owner_discord_ids = excluded.owner_discord_ids, vc_status = excluded.vc_status, \
             layouts = excluded.layouts, \
             emoji_hex = excluded.emoji_hex, emoji_hex_applied = excluded.emoji_hex_applied, \
             avatar_managed = excluded.avatar_managed, \
             avatar_hex_applied = excluded.avatar_hex_applied, \
             avatar_custom_path = excluded.avatar_custom_path, \
             avatar_custom_applied = excluded.avatar_custom_applied, \
             theme_retry_at = excluded.theme_retry_at, theme_warning = excluded.theme_warning, \
             theme_backoff = excluded.theme_backoff, \
             commands_hash = excluded.commands_hash, updated_at = excluded.updated_at",
    )
    .bind(&s.app_id)
    .bind(&s.display_name)
    .bind(s.mode.as_str())
    .bind(single)
    .bind(statuses)
    .bind(s.status_rotate_secs as i64)
    .bind(s.default_volume as i64)
    .bind(s.idle_timeout_secs as i64)
    .bind(allowed)
    .bind(owners)
    .bind(s.vc_status as i64)
    .bind(layouts)
    .bind(&s.emoji_hex)
    .bind(&s.emoji_hex_applied)
    .bind(s.avatar_managed as i64)
    .bind(&s.avatar_hex_applied)
    .bind(&s.avatar_custom_path)
    .bind(s.avatar_custom_applied as i64)
    .bind(s.theme_retry_at)
    .bind(&s.theme_warning)
    .bind(s.theme_backoff as i64)
    .bind(&s.commands_hash)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

/// Per-(identity, guild) settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuildSettings {
    pub app_id: String,
    pub guild_id: String,
    /// Roles that may control shared playback; empty = anyone in the channel.
    pub dj_role_ids: Vec<String>,
    pub controller_channel_id: Option<String>,
    pub controller_message_id: Option<String>,
    /// `None` = the bot's default volume.
    pub volume: Option<u8>,
    pub normalize: bool,
    /// The server's own choices (a command, a button)…
    pub always_on: bool,
    pub always_on_channel_id: Option<String>,
    pub autoplay: bool,
    pub announce: bool,
    /// …inside what the library owner allows this server.
    pub can_always_on: bool,
    pub can_autoplay: bool,
    /// Messages after the controller before it is re-posted at the bottom; 0 = never.
    pub announce_after: u32,
    /// How a track gets skipped: by one person allowed to, or by a vote among the listeners.
    pub skip_mode: SkipMode,
    /// The share of listeners (people in the voice channel, bots aside) a vote needs, 1 to 100.
    pub vote_percent: u8,
    /// The server's equalizer: the web client's model, applied to what the bot plays.
    pub eq: EqConfig,
    /// Post what the session was when the bot leaves after playing.
    pub summary: bool,
    /// Seconds each track blends into the next; 0 is none.
    pub crossfade_secs: u8,
    /// Where the bot picks up after a restart.
    pub pickup: Pickup,
    /// This server's own versions of some of the bot's messages, over the bot's layouts.
    pub layout_overrides: LayoutOverrides,
}

/// How a track gets skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipMode {
    /// One person allowed to control playback skips at once.
    #[default]
    Single,
    /// Listeners vote; the track goes once enough of them have.
    Vote,
}

impl SkipMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipMode::Single => "single",
            SkipMode::Vote => "vote",
        }
    }
}

/// Where the bot picks up after a restart, when it was playing as it went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pickup {
    /// Where it left off in the track.
    #[default]
    Position,
    /// The start of the track that was playing.
    Start,
}

impl Pickup {
    pub fn as_str(self) -> &'static str {
        match self {
            Pickup::Position => "position",
            Pickup::Start => "start",
        }
    }

    /// In words, for the settings panel.
    pub fn label(self) -> &'static str {
        match self {
            Pickup::Position => "where it left off",
            Pickup::Start => "the start of the track",
        }
    }
}

impl GuildSettings {
    /// How many votes a skip needs among `listeners` people: the share, rounded up, one at least.
    pub fn votes_needed(&self, listeners: usize) -> usize {
        (listeners * self.vote_percent.clamp(1, 100) as usize)
            .div_ceil(100)
            .max(1)
    }

    /// The skip rule in words, for the settings panel.
    pub fn skip_label(&self) -> String {
        match self.skip_mode {
            SkipMode::Single => "single".to_string(),
            SkipMode::Vote => format!("vote, {}% of listeners", self.vote_percent),
        }
    }

    pub fn defaults(app_id: &str, guild_id: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            guild_id: guild_id.to_string(),
            dj_role_ids: Vec::new(),
            controller_channel_id: None,
            controller_message_id: None,
            volume: None,
            normalize: true,
            always_on: false,
            always_on_channel_id: None,
            autoplay: false,
            announce: true,
            can_always_on: true,
            can_autoplay: true,
            announce_after: 20,
            skip_mode: SkipMode::Single,
            vote_percent: 50,
            eq: eq::default_config(),
            summary: true,
            crossfade_secs: 0,
            pickup: Pickup::Position,
            layout_overrides: LayoutOverrides::default(),
        }
    }

    /// The layouts this server's messages render with: the bot's, with its own laid over.
    pub fn layouts(&self, bot: &BotLayouts) -> BotLayouts {
        bot.with_overrides(&self.layout_overrides)
    }

    /// Role ids that parse as snowflakes; the bot never wrote anything else, but a dashboard could.
    pub fn dj_roles(&self) -> Vec<u64> {
        self.dj_role_ids
            .iter()
            .filter_map(|r| r.parse::<u64>().ok())
            .collect()
    }
}

/// A partial update from the dashboard for one guild; absent means unchanged, `null` clears.
/// 24/7 and autoplay themselves are not here: the dashboard grants or withdraws them, the server
/// turns them on and off.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GuildSettingsPatch {
    pub dj_role_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub volume: Option<Option<u8>>,
    pub normalize: Option<bool>,
    pub announce: Option<bool>,
    pub announce_after: Option<u32>,
    pub can_always_on: Option<bool>,
    pub can_autoplay: Option<bool>,
    pub skip_mode: Option<SkipMode>,
    pub vote_percent: Option<u8>,
    /// Kept to the ten bands and the range on the way in.
    pub eq: Option<EqConfig>,
    pub summary: Option<bool>,
    /// Kept to twelve seconds at most.
    pub crossfade_secs: Option<u8>,
    pub pickup: Option<Pickup>,
    /// Checked against the layout rules by the API before it gets here.
    pub layout_overrides: Option<LayoutOverrides>,
}

impl GuildSettingsPatch {
    pub fn apply(self, s: &mut GuildSettings) {
        if let Some(v) = self.dj_role_ids {
            let mut list: Vec<String> = v
                .into_iter()
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
                .collect();
            list.dedup();
            s.dj_role_ids = list;
        }
        if let Some(v) = self.volume {
            s.volume = v.map(|x| x.min(150));
        }
        if let Some(v) = self.normalize {
            s.normalize = v;
        }
        if let Some(v) = self.announce {
            s.announce = v;
        }
        if let Some(v) = self.announce_after {
            s.announce_after = v.min(500);
        }
        if let Some(v) = self.can_always_on {
            s.can_always_on = v;
            if !v {
                // Withdrawn: the server's 24/7 ends with it.
                s.always_on = false;
                s.always_on_channel_id = None;
            }
        }
        if let Some(v) = self.can_autoplay {
            s.can_autoplay = v;
            if !v {
                s.autoplay = false;
            }
        }
        if let Some(v) = self.skip_mode {
            s.skip_mode = v;
        }
        if let Some(v) = self.vote_percent {
            s.vote_percent = v.clamp(1, 100);
        }
        if let Some(v) = self.eq {
            s.eq = eq::tidy(&v);
        }
        if let Some(v) = self.summary {
            s.summary = v;
        }
        if let Some(v) = self.crossfade_secs {
            s.crossfade_secs = v.min(MAX_CROSSFADE_SECS);
        }
        if let Some(v) = self.pickup {
            s.pickup = v;
        }
        if let Some(v) = self.layout_overrides {
            s.layout_overrides = v;
        }
    }
}

#[derive(sqlx::FromRow)]
struct GuildRow {
    app_id: String,
    guild_id: String,
    dj_role_ids: String,
    controller_channel_id: Option<String>,
    controller_message_id: Option<String>,
    volume: Option<i64>,
    normalize: i64,
    always_on: i64,
    always_on_channel_id: Option<String>,
    autoplay: i64,
    announce: i64,
    can_always_on: i64,
    can_autoplay: i64,
    announce_after: i64,
    skip_mode: String,
    vote_percent: i64,
    eq: Option<String>,
    summary: i64,
    crossfade_secs: i64,
    pickup: String,
    layout_overrides: Option<String>,
}

impl From<GuildRow> for GuildSettings {
    fn from(r: GuildRow) -> Self {
        GuildSettings {
            app_id: r.app_id,
            guild_id: r.guild_id,
            dj_role_ids: serde_json::from_str(&r.dj_role_ids).unwrap_or_default(),
            controller_channel_id: r.controller_channel_id,
            controller_message_id: r.controller_message_id,
            volume: r.volume.map(|v| v.clamp(0, 150) as u8),
            normalize: r.normalize != 0,
            always_on: r.always_on != 0,
            always_on_channel_id: r.always_on_channel_id,
            autoplay: r.autoplay != 0,
            announce: r.announce != 0,
            can_always_on: r.can_always_on != 0,
            can_autoplay: r.can_autoplay != 0,
            announce_after: r.announce_after.clamp(0, 500) as u32,
            skip_mode: if r.skip_mode == "vote" {
                SkipMode::Vote
            } else {
                SkipMode::Single
            },
            vote_percent: r.vote_percent.clamp(1, 100) as u8,
            eq: r
                .eq
                .as_deref()
                .and_then(|j| serde_json::from_str::<EqConfig>(j).ok())
                .map(|c| eq::tidy(&c))
                .unwrap_or_else(eq::default_config),
            summary: r.summary != 0,
            crossfade_secs: r.crossfade_secs.clamp(0, MAX_CROSSFADE_SECS as i64) as u8,
            pickup: if r.pickup == "start" {
                Pickup::Start
            } else {
                Pickup::Position
            },
            layout_overrides: r
                .layout_overrides
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .map(|mut o: LayoutOverrides| {
                    template::upgrade_overrides(&mut o);
                    o
                })
                .unwrap_or_default(),
        }
    }
}

const GUILD_COLS: &str = "app_id, guild_id, dj_role_ids, controller_channel_id, \
     controller_message_id, volume, normalize, always_on, always_on_channel_id, autoplay, announce, \
     can_always_on, can_autoplay, announce_after, skip_mode, vote_percent, eq, summary, \
     crossfade_secs, pickup, layout_overrides";

pub async fn load_guild(db: &SqlitePool, app_id: &str, guild_id: &str) -> AppResult<GuildSettings> {
    let row = sqlx::query_as::<_, GuildRow>(AssertSqlSafe(format!(
        "SELECT {GUILD_COLS} FROM discord_guild_settings WHERE app_id = ? AND guild_id = ?"
    )))
    .bind(app_id)
    .bind(guild_id)
    .fetch_optional(db)
    .await?;
    Ok(row
        .map(GuildSettings::from)
        .unwrap_or_else(|| GuildSettings::defaults(app_id, guild_id)))
}

/// Every guild row for one identity — what the dashboard lists and what 24/7 rejoins at boot.
pub async fn load_guilds(db: &SqlitePool, app_id: &str) -> AppResult<Vec<GuildSettings>> {
    let rows = sqlx::query_as::<_, GuildRow>(AssertSqlSafe(format!(
        "SELECT {GUILD_COLS} FROM discord_guild_settings WHERE app_id = ?"
    )))
    .bind(app_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(GuildSettings::from).collect())
}

pub async fn save_guild(db: &SqlitePool, s: &GuildSettings) -> AppResult<()> {
    let dj = serde_json::to_string(&s.dj_role_ids).unwrap_or_else(|_| "[]".into());
    let overrides = (!s.layout_overrides.is_empty())
        .then(|| serde_json::to_string(&s.layout_overrides).ok())
        .flatten();
    sqlx::query(
        "INSERT INTO discord_guild_settings (app_id, guild_id, dj_role_ids, controller_channel_id, \
             controller_message_id, volume, normalize, always_on, always_on_channel_id, autoplay, \
             announce, can_always_on, can_autoplay, announce_after, skip_mode, vote_percent, \
             eq, summary, crossfade_secs, pickup, layout_overrides, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(app_id, guild_id) DO UPDATE SET \
             dj_role_ids = excluded.dj_role_ids, \
             controller_channel_id = excluded.controller_channel_id, \
             controller_message_id = excluded.controller_message_id, volume = excluded.volume, \
             normalize = excluded.normalize, always_on = excluded.always_on, \
             always_on_channel_id = excluded.always_on_channel_id, autoplay = excluded.autoplay, \
             announce = excluded.announce, can_always_on = excluded.can_always_on, \
             can_autoplay = excluded.can_autoplay, announce_after = excluded.announce_after, \
             skip_mode = excluded.skip_mode, vote_percent = excluded.vote_percent, \
             eq = excluded.eq, summary = excluded.summary, \
             crossfade_secs = excluded.crossfade_secs, pickup = excluded.pickup, \
             layout_overrides = excluded.layout_overrides, updated_at = excluded.updated_at",
    )
    .bind(&s.app_id)
    .bind(&s.guild_id)
    .bind(dj)
    .bind(&s.controller_channel_id)
    .bind(&s.controller_message_id)
    .bind(s.volume.map(|v| v as i64))
    .bind(s.normalize as i64)
    .bind(s.always_on as i64)
    .bind(&s.always_on_channel_id)
    .bind(s.autoplay as i64)
    .bind(s.announce as i64)
    .bind(s.can_always_on as i64)
    .bind(s.can_autoplay as i64)
    .bind(s.announce_after as i64)
    .bind(s.skip_mode.as_str())
    .bind(s.vote_percent as i64)
    .bind(serde_json::to_string(&s.eq).ok())
    .bind(s.summary as i64)
    .bind(s.crossfade_secs as i64)
    .bind(s.pickup.as_str())
    .bind(overrides)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

/// Log one play for `/stats`. Returns the row id so `ms_played` can be finalised when the track
/// ends.
pub async fn record_play(
    db: &SqlitePool,
    app_id: &str,
    guild_id: &str,
    track_id: &str,
    requested_by: Option<u64>,
    listeners: u32,
) -> AppResult<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO discord_plays (app_id, guild_id, track_id, requested_by, started_at, \
             ms_played, listeners) VALUES (?, ?, ?, ?, ?, 0, ?) RETURNING id",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(track_id)
    .bind(requested_by.map(|u| u.to_string()))
    .bind(now_ms())
    .bind(listeners as i64)
    .fetch_one(db)
    .await?;
    Ok(id)
}

/// What one server played through the bot in a window, for `/stats`.
pub struct GuildStats {
    pub plays: i64,
    pub ms: i64,
    /// `(title, artist, plays)`, most played first.
    pub top_tracks: Vec<(String, String, i64)>,
    /// `(discord user id, plays)`, most first.
    pub top_requesters: Vec<(String, i64)>,
}

pub async fn guild_stats(
    db: &SqlitePool,
    app_id: &str,
    guild_id: &str,
    since_ms: i64,
    requested_by: Option<u64>,
) -> AppResult<GuildStats> {
    let who = requested_by.map(|u| u.to_string());
    let (plays, ms): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(ms_played), 0) FROM discord_plays \
         WHERE app_id = ? AND guild_id = ? AND started_at >= ? \
           AND (? IS NULL OR requested_by = ?)",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(since_ms)
    .bind(&who)
    .bind(&who)
    .fetch_one(db)
    .await?;
    let top_tracks: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT t.title, COALESCE(ar.name, ''), COUNT(*) AS n FROM discord_plays p \
         JOIN tracks t ON t.id = p.track_id \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         WHERE p.app_id = ? AND p.guild_id = ? AND p.started_at >= ? \
           AND (? IS NULL OR p.requested_by = ?) \
         GROUP BY p.track_id ORDER BY n DESC, SUM(p.ms_played) DESC LIMIT 5",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(since_ms)
    .bind(&who)
    .bind(&who)
    .fetch_all(db)
    .await?;
    let top_requesters: Vec<(String, i64)> = sqlx::query_as(
        "SELECT requested_by, COUNT(*) AS n FROM discord_plays \
         WHERE app_id = ? AND guild_id = ? AND started_at >= ? AND requested_by IS NOT NULL \
         GROUP BY requested_by ORDER BY n DESC LIMIT 3",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(since_ms)
    .fetch_all(db)
    .await?;
    Ok(GuildStats {
        plays,
        ms,
        top_tracks,
        top_requesters,
    })
}

/// Note how many listeners a play was queued to count for.
pub async fn mark_scrobbled(db: &SqlitePool, play_id: i64, listeners: usize) -> AppResult<()> {
    sqlx::query("UPDATE discord_plays SET scrobbled_for = ? WHERE id = ?")
        .bind(listeners as i64)
        .bind(play_id)
        .execute(db)
        .await?;
    Ok(())
}

/// One line of `/history`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlayEntry {
    pub title: String,
    pub artist: String,
    pub requested_by: Option<String>,
    pub started_at: i64,
    pub ms_played: i64,
    pub scrobbled_for: i64,
    /// How many were in the channel when it started.
    pub listeners: i64,
}

/// What a session came to, from its plays.
#[derive(Debug, Clone, Default)]
pub struct SessionFacts {
    pub count: usize,
    pub total_ms: u64,
    pub peak_listeners: u32,
    pub requesters: usize,
    pub since_ms: i64,
}

impl SessionFacts {
    pub fn of(plays: &[PlayEntry], since_ms: i64) -> Self {
        let mut requesters: Vec<&str> = plays
            .iter()
            .filter_map(|p| p.requested_by.as_deref())
            .collect();
        requesters.sort_unstable();
        requesters.dedup();
        SessionFacts {
            count: plays.len(),
            total_ms: plays.iter().map(|p| p.ms_played.max(0) as u64).sum(),
            peak_listeners: plays
                .iter()
                .map(|p| p.listeners.max(0) as u32)
                .max()
                .unwrap_or(0),
            requesters: requesters.len(),
            since_ms,
        }
    }
}

/// The most a crossfade may overlap, in seconds.
pub const MAX_CROSSFADE_SECS: u8 = 12;

/// The plays of a session, oldest first: everything since `since_ms`, up to `limit`.
pub async fn plays_since(
    db: &SqlitePool,
    app_id: &str,
    guild_id: &str,
    since_ms: i64,
    limit: i64,
) -> AppResult<Vec<PlayEntry>> {
    Ok(sqlx::query_as::<_, PlayEntry>(
        "SELECT COALESCE(t.title, '?') AS title, COALESCE(ar.name, '') AS artist, \
                p.requested_by, p.started_at, p.ms_played, p.scrobbled_for, p.listeners \
         FROM discord_plays p \
         LEFT JOIN tracks t ON t.id = p.track_id \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         WHERE p.app_id = ? AND p.guild_id = ? AND p.started_at >= ? \
         ORDER BY p.started_at ASC LIMIT ?",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(since_ms)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Every server this bot was busy in when it went down, with what it was doing, as JSON.
pub async fn load_queues(db: &SqlitePool, app_id: &str) -> AppResult<Vec<(String, String)>> {
    Ok(sqlx::query_as::<_, (String, String)>(
        "SELECT guild_id, state FROM discord_saved_queue WHERE app_id = ?",
    )
    .bind(app_id)
    .fetch_all(db)
    .await?)
}

/// What the bot was doing in a server when it went down, as JSON, for the rejoin.
pub async fn save_queue(
    db: &SqlitePool,
    app_id: &str,
    guild_id: &str,
    state: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO discord_saved_queue (app_id, guild_id, state, saved_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(app_id, guild_id) DO UPDATE SET state = excluded.state, saved_at = excluded.saved_at",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(state)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn load_queue(
    db: &SqlitePool,
    app_id: &str,
    guild_id: &str,
) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT state FROM discord_saved_queue WHERE app_id = ? AND guild_id = ?",
    )
    .bind(app_id)
    .bind(guild_id)
    .fetch_optional(db)
    .await?)
}

pub async fn clear_queue(db: &SqlitePool, app_id: &str, guild_id: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM discord_saved_queue WHERE app_id = ? AND guild_id = ?")
        .bind(app_id)
        .bind(guild_id)
        .execute(db)
        .await?;
    Ok(())
}

/// The last `limit` plays in a guild, newest first, from the persistent log rather than the
/// player's in-memory history: a stop or a loop does not erase what was heard.
pub async fn recent_plays(
    db: &SqlitePool,
    app_id: &str,
    guild_id: &str,
    limit: i64,
) -> AppResult<Vec<PlayEntry>> {
    Ok(sqlx::query_as::<_, PlayEntry>(
        "SELECT COALESCE(t.title, '?') AS title, COALESCE(ar.name, '') AS artist, \
                p.requested_by, p.started_at, p.ms_played, p.scrobbled_for, p.listeners \
         FROM discord_plays p \
         LEFT JOIN tracks t ON t.id = p.track_id \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         WHERE p.app_id = ? AND p.guild_id = ? \
         ORDER BY p.started_at DESC LIMIT ?",
    )
    .bind(app_id)
    .bind(guild_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

pub async fn finish_play(db: &SqlitePool, play_id: i64, ms_played: u64) -> AppResult<()> {
    sqlx::query("UPDATE discord_plays SET ms_played = ? WHERE id = ?")
        .bind(ms_played as i64)
        .bind(play_id)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_null_clears_and_absent_keeps() {
        let mut s = BotSettings::defaults("1");
        s.display_name = Some("Chordia".into());
        s.allowed_guilds = Some(vec!["9".into()]);

        let p: BotSettingsPatch = serde_json::from_str(r#"{"display_name": null}"#).unwrap();
        p.apply(&mut s);
        assert_eq!(s.display_name, None);
        assert_eq!(s.allowed_guilds.as_deref(), Some(&["9".to_string()][..]));

        let p: BotSettingsPatch =
            serde_json::from_str(r#"{"display_name": "Two", "default_volume": 200}"#).unwrap();
        p.apply(&mut s);
        assert_eq!(s.display_name.as_deref(), Some("Two"));
        assert_eq!(s.default_volume, 150);
    }

    #[test]
    fn statuses_are_trimmed_capped_and_never_empty() {
        let mut s = BotSettings::defaults("1");
        let p: BotSettingsPatch = serde_json::from_str(
            r#"{"multi_statuses": ["  ", "/play", "{servers} servers"], "status_rotate_secs": 1}"#,
        )
        .unwrap();
        p.apply(&mut s);
        assert_eq!(s.multi_statuses, vec!["/play", "{servers} servers"]);
        assert_eq!(s.status_rotate_secs, MIN_ROTATE_SECS);
        let p: BotSettingsPatch =
            serde_json::from_str(r#"{"multi_statuses": [], "single_statuses": [" "]}"#).unwrap();
        p.apply(&mut s);
        assert_eq!(s.multi_statuses, vec![MULTI_DEFAULT]);
        assert_eq!(s.single_statuses, vec![SINGLE_DEFAULT]);
    }

    #[test]
    fn withdrawing_a_permission_ends_its_use() {
        let mut g = GuildSettings::defaults("1", "2");
        g.always_on = true;
        g.always_on_channel_id = Some("5".into());
        g.autoplay = true;
        let p: GuildSettingsPatch = serde_json::from_str(
            r#"{"can_always_on": false, "can_autoplay": false, "dj_role_ids": ["1", "x", "1", " 2 "]}"#,
        )
        .unwrap();
        p.apply(&mut g);
        assert!(!g.always_on && g.always_on_channel_id.is_none() && !g.autoplay);
        assert_eq!(g.dj_role_ids, vec!["1", "2"]);
        assert_eq!(g.dj_roles(), vec![1, 2]);
    }

    #[test]
    fn guild_allow_list() {
        let mut s = BotSettings::defaults("1");
        // The default denies: an invite the owner never allowed serves nobody.
        assert!(!s.allows_guild(5));
        s.allowed_guilds = Some(Vec::new());
        assert!(!s.allows_guild(5));
        s.allowed_guilds = Some(vec!["5".into()]);
        assert!(s.allows_guild(5));
        assert!(!s.allows_guild(6));
    }
}
