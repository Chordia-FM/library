//! One bot identity: a token, and everything that exists because of it.
//!
//! An `Identity` outlives any single gateway connection. The supervisor in `client.rs` builds a
//! serenity client for it, runs it until it exits, and rebuilds it; the identity keeps the settings,
//! the per-guild players and the profile across those restarts, and hands out the connection-scoped
//! handles (`Http`, `Cache`, `Songbird`) as `Option`s that are `None` between connections.
//!
//! Locks here are `std::sync` because every critical section is a field read or a map lookup —
//! never an await — which also lets songbird's event thread and snapshot code read them without an
//! async context.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serenity::all::{Cache, ChannelId, ChannelType, Context, GuildId, Http, ShardManager, UserId};
use songbird::driver::Scheduler;
use songbird::Songbird;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::discord::emoji::IconSet;
use crate::discord::player::GuildPlayer;
use crate::discord::settings::{self, BotSettings};
use crate::http::AppState;

/// What Discord told us about the application behind a token.
#[derive(Debug, Clone)]
pub struct Profile {
    pub app_id: u64,
    pub user_id: UserId,
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not yet connected for the first time.
    Starting,
    /// Client built, gateway connecting.
    Connecting,
    Online,
    /// Last attempt failed; the supervisor will retry.
    Failed(String),
    /// Shut down on purpose.
    Stopped,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Connecting => "connecting",
            Status::Online => "online",
            Status::Failed(_) => "failed",
            Status::Stopped => "stopped",
        }
    }
}

pub struct Identity {
    /// Position in the configured token list; part of every custom id.
    pub index: u8,
    pub(crate) token: String,
    pub state: AppState,
    /// Shared across identities so N bots share one pool of mixer threads.
    pub scheduler: Scheduler,
    pub cancel: CancellationToken,
    pub command_guilds: Vec<u64>,
    profile: RwLock<Option<Profile>>,
    status: RwLock<Status>,
    settings: RwLock<BotSettings>,
    http: RwLock<Option<Arc<Http>>>,
    cache: RwLock<Option<Arc<Cache>>>,
    songbird: RwLock<Option<Arc<Songbird>>>,
    shard_manager: RwLock<Option<Arc<ShardManager>>>,
    /// A gateway context, kept from the Ready event so presence can be set outside event handlers.
    ctx: RwLock<Option<Context>>,
    players: Mutex<HashMap<GuildId, Arc<GuildPlayer>>>,
    restart: Notify,
    /// Last presence text sent, to skip duplicate updates.
    pub(crate) last_activity: Mutex<Option<String>>,
    /// Last voice-channel status sent per channel, same reason.
    pub(crate) last_vc_status: Mutex<HashMap<ChannelId, String>>,
    /// The bot's application emojis, resolved on connect; empty until then (views fall back to
    /// Unicode glyphs).
    icons: RwLock<Arc<IconSet>>,
    /// Held while the theme job (emoji set + avatar) runs, so a ticker beat and a dashboard request
    /// never race each other into Discord's rate limits.
    pub(crate) theme_lock: tokio::sync::Mutex<()>,
    /// Which multi-server status is showing and since when.
    status_rotation: Mutex<(usize, Instant)>,
    /// Library track count for the `{tracks}` status variable.
    pub(crate) track_count: Mutex<Option<(Instant, u64)>>,
    /// Users looked up for the dashboard's owner pills, kept for a while so a dashboard poll
    /// does not become a REST call per pill.
    user_cache: Mutex<HashMap<u64, (Instant, DiscordUser)>>,
}

/// The public face of a Discord user, for the dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscordUser {
    pub id: String,
    /// Display name (the global name when set, else the username).
    pub name: String,
    pub username: String,
    pub avatar_url: String,
}

/// A guild role, for the dashboard's DJ-role picker.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RoleInfo {
    pub id: String,
    pub name: String,
    /// `#rrggbb`, absent for the default (colourless) role colour.
    pub color: Option<String>,
    pub position: u16,
}

/// A guild channel, for the dashboard's `#` autocomplete.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    /// `text`, `voice`, `stage`, `news`, `forum` or `category`.
    pub kind: &'static str,
    pub position: u16,
    /// The category it sits under, when it does.
    pub parent_id: Option<String>,
}

const USER_CACHE_TTL: Duration = Duration::from_secs(3600);

impl Identity {
    pub fn new(
        index: u8,
        token: String,
        state: AppState,
        scheduler: Scheduler,
        cancel: CancellationToken,
        command_guilds: Vec<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            index,
            token,
            state,
            scheduler,
            cancel,
            command_guilds,
            profile: RwLock::new(None),
            status: RwLock::new(Status::Starting),
            settings: RwLock::new(BotSettings::defaults("")),
            http: RwLock::new(None),
            cache: RwLock::new(None),
            songbird: RwLock::new(None),
            shard_manager: RwLock::new(None),
            ctx: RwLock::new(None),
            players: Mutex::new(HashMap::new()),
            restart: Notify::new(),
            last_activity: Mutex::new(None),
            last_vc_status: Mutex::new(HashMap::new()),
            icons: RwLock::new(Arc::new(IconSet::default())),
            theme_lock: tokio::sync::Mutex::new(()),
            status_rotation: Mutex::new((0, Instant::now())),
            track_count: Mutex::new(None),
            user_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Whether a Discord user owns this bot: on the bot's own owner list, or the owner of the
    /// library itself (through the Discord account linked to their Chordia account).
    pub fn is_owner(&self, user: u64) -> bool {
        if self.settings().is_owner(user) {
            return true;
        }
        crate::discord::runtime().is_some_and(|rt| rt.owner_discord_id() == Some(user))
    }

    /// The index of the multi-server status to show now, advancing every `rotate_secs`.
    pub fn status_slot(&self, len: usize, rotate_secs: u32) -> usize {
        if len == 0 {
            return 0;
        }
        let mut rot = self
            .status_rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if rot.1.elapsed() >= Duration::from_secs(rotate_secs.max(1) as u64) {
            rot.0 = (rot.0 + 1) % len;
            rot.1 = Instant::now();
        }
        rot.0 % len
    }

    /// A user by id, from the cache or from Discord (a bot may look up any user by id).
    pub async fn lookup_user(&self, id: u64) -> Option<DiscordUser> {
        {
            let cache = self.user_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, u)) = cache.get(&id) {
                if at.elapsed() < USER_CACHE_TTL {
                    return Some(u.clone());
                }
            }
        }
        let http = self.http()?;
        let user = http.get_user(UserId::new(id)).await.ok()?;
        let info = DiscordUser {
            id: id.to_string(),
            name: user
                .global_name
                .clone()
                .unwrap_or_else(|| user.name.clone()),
            username: user.name.clone(),
            avatar_url: user.face(),
        };
        self.user_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (Instant::now(), info.clone()));
        Some(info)
    }

    /// A guild's roles for the DJ-role picker: everything but `@everyone` and roles Discord
    /// manages for integrations, highest first.
    pub fn guild_roles(&self, guild: GuildId) -> Vec<RoleInfo> {
        let Some(cache) = self.cache() else {
            return Vec::new();
        };
        let Some(g) = cache.guild(guild) else {
            return Vec::new();
        };
        let mut roles: Vec<RoleInfo> = g
            .roles
            .values()
            .filter(|r| r.id.get() != guild.get() && !r.managed)
            .map(|r| RoleInfo {
                id: r.id.get().to_string(),
                name: r.name.clone(),
                color: (r.colour.0 != 0).then(|| format!("#{:06x}", r.colour.0)),
                position: r.position,
            })
            .collect();
        roles.sort_by(|a, b| {
            b.position
                .cmp(&a.position)
                .then_with(|| a.name.cmp(&b.name))
        });
        roles
    }

    pub fn guild_icon_url(&self, guild: GuildId) -> Option<String> {
        let cache = self.cache()?;
        cache.guild(guild).and_then(|g| g.icon_url())
    }

    /// A guild's channels a message could mention, categories first then by position.
    pub fn guild_channels(&self, guild: GuildId) -> Vec<ChannelInfo> {
        use serenity::all::ChannelType;
        let Some(cache) = self.cache() else {
            return Vec::new();
        };
        let Some(g) = cache.guild(guild) else {
            return Vec::new();
        };
        let mut out: Vec<ChannelInfo> = g
            .channels
            .values()
            .filter_map(|c| {
                let kind = match c.kind {
                    ChannelType::Text => "text",
                    ChannelType::Voice => "voice",
                    ChannelType::Stage => "stage",
                    ChannelType::News => "news",
                    ChannelType::Forum => "forum",
                    ChannelType::Category => "category",
                    _ => return None,
                };
                Some(ChannelInfo {
                    id: c.id.get().to_string(),
                    name: c.name.clone(),
                    kind,
                    position: c.position,
                    parent_id: c.parent_id.map(|p| p.get().to_string()),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            (a.kind != "category")
                .cmp(&(b.kind != "category"))
                .then(a.position.cmp(&b.position))
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// Members whose name starts with `query`, for the dashboard's `@` autocomplete. A gateway
    /// search, so it needs no privileged intent.
    pub async fn search_members(&self, guild: GuildId, query: &str) -> Vec<DiscordUser> {
        let Some(http) = self.http() else {
            return Vec::new();
        };
        let members = guild
            .search_members(&http, query, Some(10))
            .await
            .unwrap_or_default();
        members
            .into_iter()
            .map(|m| DiscordUser {
                id: m.user.id.get().to_string(),
                name: m.display_name().to_string(),
                username: m.user.name.clone(),
                avatar_url: m.face(),
            })
            .collect()
    }

    pub fn icons(&self) -> Arc<IconSet> {
        self.icons.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_icons(&self, set: Arc<IconSet>) {
        *self.icons.write().unwrap_or_else(|e| e.into_inner()) = set;
    }

    // ---- profile / status / settings ---------------------------------------------------------------

    pub fn profile(&self) -> Option<Profile> {
        self.profile
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_profile(&self, p: Profile) {
        *self.profile.write().unwrap_or_else(|e| e.into_inner()) = Some(p);
    }

    pub fn app_id_sync(&self) -> Option<u64> {
        self.profile().map(|p| p.app_id)
    }

    pub fn user_id(&self) -> Option<UserId> {
        self.profile().map(|p| p.user_id)
    }

    pub fn status(&self) -> Status {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_status(&self, s: Status) {
        *self.status.write().unwrap_or_else(|e| e.into_inner()) = s;
    }

    pub fn settings(&self) -> BotSettings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_settings(&self, s: BotSettings) {
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// Load this identity's settings once the application id is known.
    pub async fn load_settings(&self) {
        let Some(app_id) = self.app_id_sync() else {
            return;
        };
        match settings::load_bot(&self.state.db, &app_id.to_string()).await {
            Ok(s) => self.set_settings(s),
            Err(e) => tracing::warn!(error = %e, "loading bot settings; using defaults"),
        }
    }

    pub async fn save_settings(&self) {
        let s = self.settings();
        if s.app_id.is_empty() {
            return;
        }
        if let Err(e) = settings::save_bot(&self.state.db, &s).await {
            tracing::warn!(error = %e, "saving bot settings");
        }
    }

    /// The name views show: the dashboard override, else the bot user's name, else a placeholder.
    pub fn display_name_sync(&self) -> String {
        if let Some(n) = self.settings().display_name {
            return n;
        }
        if let Some(p) = self.profile() {
            return p.name;
        }
        format!("Chordia {}", self.index + 1)
    }

    // ---- connection handles ------------------------------------------------------------------------

    pub fn http(&self) -> Option<Arc<Http>> {
        self.http.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn cache(&self) -> Option<Arc<Cache>> {
        self.cache.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn songbird(&self) -> Option<Arc<Songbird>> {
        self.songbird
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn shard_manager(&self) -> Option<Arc<ShardManager>> {
        self.shard_manager
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn context(&self) -> Option<Context> {
        self.ctx.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn attach_connection(
        &self,
        http: Arc<Http>,
        cache: Arc<Cache>,
        songbird: Arc<Songbird>,
        shard_manager: Arc<ShardManager>,
    ) {
        *self.http.write().unwrap_or_else(|e| e.into_inner()) = Some(http);
        *self.cache.write().unwrap_or_else(|e| e.into_inner()) = Some(cache);
        *self.songbird.write().unwrap_or_else(|e| e.into_inner()) = Some(songbird);
        *self
            .shard_manager
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(shard_manager);
    }

    pub(crate) fn set_context(&self, ctx: Context) {
        *self.ctx.write().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
    }

    pub(crate) fn detach_connection(&self) {
        *self.http.write().unwrap_or_else(|e| e.into_inner()) = None;
        *self.cache.write().unwrap_or_else(|e| e.into_inner()) = None;
        *self.songbird.write().unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .shard_manager
            .write()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self.ctx.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.last_activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.last_vc_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Ask the supervisor to tear the gateway down and reconnect.
    pub fn request_restart(&self) {
        self.restart.notify_one();
    }

    pub(crate) async fn restart_requested(&self) {
        self.restart.notified().await;
    }

    // ---- players -----------------------------------------------------------------------------------

    /// The player for a guild, created on first use with that guild's saved settings.
    pub async fn player(self: &Arc<Self>, guild: GuildId) -> Arc<GuildPlayer> {
        if let Some(p) = self.player_arc(guild) {
            return p;
        }
        let app_id = self.app_id_sync().unwrap_or(0).to_string();
        let gs = settings::load_guild(&self.state.db, &app_id, &guild.to_string())
            .await
            .unwrap_or_else(|_| settings::GuildSettings::defaults(&app_id, &guild.to_string()));
        let default_volume = self.settings().default_volume;
        let mut players = self.players.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            players
                .entry(guild)
                .or_insert_with(|| GuildPlayer::new(self, guild, gs, default_volume)),
        )
    }

    pub fn player_arc(&self, guild: GuildId) -> Option<Arc<GuildPlayer>> {
        self.players
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&guild)
            .cloned()
    }

    pub fn players(&self) -> Vec<Arc<GuildPlayer>> {
        self.players
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// The web client to link to, when there is one: the library must be paired to a Hub and
    /// publishing its catalog there, otherwise the pages a link would open do not exist.
    pub async fn web_base(&self) -> Option<String> {
        if self.state.config.metadata_storage != crate::config::MetadataStorage::Hub {
            return None;
        }
        if self.state.credentials.read().await.is_none() {
            return None;
        }
        let base = self.state.config.frontend_url.trim_end_matches('/');
        (!base.is_empty()).then(|| base.to_string())
    }

    // ---- cache lookups -----------------------------------------------------------------------------

    /// A voice channel's configured bitrate in kbps, from the cache.
    pub fn channel_bitrate_kbps(&self, guild: GuildId, channel: ChannelId) -> Option<u32> {
        let cache = self.cache()?;
        let guild = cache.guild(guild)?;
        guild.channels.get(&channel)?.bitrate.map(|b| b / 1000)
    }

    pub fn channel_name(&self, guild: GuildId, channel: ChannelId) -> Option<String> {
        let cache = self.cache()?;
        let guild = cache.guild(guild)?;
        guild.channels.get(&channel).map(|c| c.name.clone())
    }

    pub fn guild_name(&self, guild: GuildId) -> Option<String> {
        let cache = self.cache()?;
        cache.guild(guild).map(|g| g.name.clone())
    }

    /// The voice channel a member is in right now, from the cache.
    pub fn member_voice_channel(&self, guild: GuildId, user: UserId) -> Option<ChannelId> {
        let cache = self.cache()?;
        let guild = cache.guild(guild)?;
        guild.voice_states.get(&user)?.channel_id
    }

    pub fn is_voice_channel(&self, guild: GuildId, channel: ChannelId) -> bool {
        let Some(cache) = self.cache() else {
            return true;
        };
        let Some(guild) = cache.guild(guild) else {
            return true;
        };
        guild
            .channels
            .get(&channel)
            .map(|c| matches!(c.kind, ChannelType::Voice | ChannelType::Stage))
            .unwrap_or(true)
    }

    /// Recompute who is listening in a player's channel from the cache and tell the player.
    pub async fn refresh_listeners(&self, player: &GuildPlayer) {
        let Some(vc) = player.voice_channel().await else {
            return;
        };
        let me = self.user_id();
        let users: HashSet<UserId> = match self.cache().and_then(|c| {
            c.guild(player.guild_id).map(|g| {
                g.voice_states
                    .values()
                    .filter(|v| v.channel_id == Some(vc))
                    .filter(|v| Some(v.user_id) != me)
                    .filter(|v| !v.member.as_ref().is_some_and(|m| m.user.bot))
                    .map(|v| v.user_id)
                    .collect()
            })
        }) {
            Some(u) => u,
            None => return,
        };
        player.set_listeners(users).await;
    }

    /// Guilds the bot is in, from the cache.
    pub fn guild_ids(&self) -> Vec<GuildId> {
        self.cache().map(|c| c.guilds()).unwrap_or_default()
    }

    /// The invite link: the `bot` and `applications.commands` scopes and no permissions at all.
    /// What the bot may do in a server is the server's to grant, through its roles and channel
    /// overrides, the way any member is trusted: it needs to see and post in the text channel the
    /// controller lives in (with links and files), and to connect and speak in voice. Setting the
    /// voice channel's status is optional; without it that one call fails quietly.
    pub fn invite_url(app_id: u64) -> String {
        format!(
            "https://discord.com/oauth2/authorize?client_id={app_id}&scope=bot%20applications.commands&permissions=0"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_invite_asks_for_the_scopes_and_no_permissions() {
        assert_eq!(
            Identity::invite_url(42),
            "https://discord.com/oauth2/authorize?client_id=42&scope=bot%20applications.commands&permissions=0"
        );
    }
}
