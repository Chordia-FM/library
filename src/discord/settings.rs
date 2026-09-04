//! Runtime settings for a bot identity and for each guild it serves, persisted in SQLite
//! (migration 0022). The token is the only thing about a bot that lives in the TOML file;
//! everything here is meant to be changed while the bot runs, from the dashboard or from Discord.

use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, SqlitePool};

use crate::error::AppResult;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How a bot presents itself across guilds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BotMode {
    /// Serves any guild; the presence is a static "Listening to /play".
    #[default]
    Multi,
    /// One guild is the point. Presence follows that guild's now-playing, rendered through
    /// [`BotSettings::presence_template`].
    Single,
}

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
    pub presence_template: String,
    pub default_volume: u8,
    pub idle_timeout_secs: u32,
    /// `None` = any guild the bot is invited to.
    pub allowed_guilds: Option<Vec<String>>,
    pub owner_discord_ids: Vec<String>,
    pub vc_status: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands_hash: Option<String>,
}

impl BotSettings {
    pub fn defaults(app_id: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            display_name: None,
            mode: BotMode::Multi,
            presence_template: "{title} — {artist}".to_string(),
            default_volume: 100,
            idle_timeout_secs: 300,
            allowed_guilds: None,
            owner_discord_ids: Vec::new(),
            vc_status: true,
            commands_hash: None,
        }
    }

    pub fn is_owner(&self, user_id: u64) -> bool {
        let id = user_id.to_string();
        self.owner_discord_ids.contains(&id)
    }

    pub fn allows_guild(&self, guild_id: u64) -> bool {
        match &self.allowed_guilds {
            None => true,
            Some(list) => {
                let id = guild_id.to_string();
                list.contains(&id)
            }
        }
    }
}

/// A partial update from the dashboard: every field optional, absent means unchanged.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BotSettingsPatch {
    #[serde(default, deserialize_with = "double_option")]
    pub display_name: Option<Option<String>>,
    pub mode: Option<BotMode>,
    pub presence_template: Option<String>,
    pub default_volume: Option<u8>,
    pub idle_timeout_secs: Option<u32>,
    #[serde(default, deserialize_with = "double_option")]
    pub allowed_guilds: Option<Option<Vec<String>>>,
    pub owner_discord_ids: Option<Vec<String>>,
    pub vc_status: Option<bool>,
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
        if let Some(v) = self.presence_template {
            s.presence_template = v;
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
    }
}

#[derive(sqlx::FromRow)]
struct BotRow {
    app_id: String,
    display_name: Option<String>,
    mode: String,
    presence_template: String,
    default_volume: i64,
    idle_timeout_secs: i64,
    allowed_guilds: Option<String>,
    owner_discord_ids: String,
    vc_status: i64,
    commands_hash: Option<String>,
}

pub async fn load_bot(db: &SqlitePool, app_id: &str) -> AppResult<BotSettings> {
    let row = sqlx::query_as::<_, BotRow>(
        "SELECT app_id, display_name, mode, presence_template, default_volume, \
                idle_timeout_secs, allowed_guilds, owner_discord_ids, vc_status, commands_hash \
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
            presence_template: r.presence_template,
            default_volume: r.default_volume.clamp(0, 150) as u8,
            idle_timeout_secs: r.idle_timeout_secs.max(0) as u32,
            allowed_guilds: r.allowed_guilds.and_then(|j| serde_json::from_str(&j).ok()),
            owner_discord_ids: serde_json::from_str(&r.owner_discord_ids).unwrap_or_default(),
            vc_status: r.vc_status != 0,
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
    sqlx::query(
        "INSERT INTO discord_bot_settings (app_id, display_name, mode, presence_template, \
             default_volume, idle_timeout_secs, allowed_guilds, owner_discord_ids, vc_status, \
             commands_hash, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(app_id) DO UPDATE SET \
             display_name = excluded.display_name, mode = excluded.mode, \
             presence_template = excluded.presence_template, \
             default_volume = excluded.default_volume, \
             idle_timeout_secs = excluded.idle_timeout_secs, \
             allowed_guilds = excluded.allowed_guilds, \
             owner_discord_ids = excluded.owner_discord_ids, vc_status = excluded.vc_status, \
             commands_hash = excluded.commands_hash, updated_at = excluded.updated_at",
    )
    .bind(&s.app_id)
    .bind(&s.display_name)
    .bind(s.mode.as_str())
    .bind(&s.presence_template)
    .bind(s.default_volume as i64)
    .bind(s.idle_timeout_secs as i64)
    .bind(allowed)
    .bind(owners)
    .bind(s.vc_status as i64)
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
    pub dj_role_id: Option<String>,
    pub controller_channel_id: Option<String>,
    pub controller_message_id: Option<String>,
    /// `None` = the bot's default volume.
    pub volume: Option<u8>,
    pub normalize: bool,
    pub always_on: bool,
    pub always_on_channel_id: Option<String>,
    pub autoplay: bool,
    pub announce: bool,
}

impl GuildSettings {
    pub fn defaults(app_id: &str, guild_id: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            guild_id: guild_id.to_string(),
            dj_role_id: None,
            controller_channel_id: None,
            controller_message_id: None,
            volume: None,
            normalize: true,
            always_on: false,
            always_on_channel_id: None,
            autoplay: false,
            announce: true,
        }
    }
}

#[derive(sqlx::FromRow)]
struct GuildRow {
    app_id: String,
    guild_id: String,
    dj_role_id: Option<String>,
    controller_channel_id: Option<String>,
    controller_message_id: Option<String>,
    volume: Option<i64>,
    normalize: i64,
    always_on: i64,
    always_on_channel_id: Option<String>,
    autoplay: i64,
    announce: i64,
}

impl From<GuildRow> for GuildSettings {
    fn from(r: GuildRow) -> Self {
        GuildSettings {
            app_id: r.app_id,
            guild_id: r.guild_id,
            dj_role_id: r.dj_role_id,
            controller_channel_id: r.controller_channel_id,
            controller_message_id: r.controller_message_id,
            volume: r.volume.map(|v| v.clamp(0, 150) as u8),
            normalize: r.normalize != 0,
            always_on: r.always_on != 0,
            always_on_channel_id: r.always_on_channel_id,
            autoplay: r.autoplay != 0,
            announce: r.announce != 0,
        }
    }
}

const GUILD_COLS: &str = "app_id, guild_id, dj_role_id, controller_channel_id, \
     controller_message_id, volume, normalize, always_on, always_on_channel_id, autoplay, announce";

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
    sqlx::query(
        "INSERT INTO discord_guild_settings (app_id, guild_id, dj_role_id, controller_channel_id, \
             controller_message_id, volume, normalize, always_on, always_on_channel_id, autoplay, \
             announce, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(app_id, guild_id) DO UPDATE SET \
             dj_role_id = excluded.dj_role_id, \
             controller_channel_id = excluded.controller_channel_id, \
             controller_message_id = excluded.controller_message_id, volume = excluded.volume, \
             normalize = excluded.normalize, always_on = excluded.always_on, \
             always_on_channel_id = excluded.always_on_channel_id, autoplay = excluded.autoplay, \
             announce = excluded.announce, updated_at = excluded.updated_at",
    )
    .bind(&s.app_id)
    .bind(&s.guild_id)
    .bind(&s.dj_role_id)
    .bind(&s.controller_channel_id)
    .bind(&s.controller_message_id)
    .bind(s.volume.map(|v| v as i64))
    .bind(s.normalize as i64)
    .bind(s.always_on as i64)
    .bind(&s.always_on_channel_id)
    .bind(s.autoplay as i64)
    .bind(s.announce as i64)
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
    fn guild_allow_list() {
        let mut s = BotSettings::defaults("1");
        assert!(s.allows_guild(5));
        s.allowed_guilds = Some(vec!["5".into()]);
        assert!(s.allows_guild(5));
        assert!(!s.allows_guild(6));
    }
}
