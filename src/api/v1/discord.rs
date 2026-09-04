//! Management API for the Discord bot(s): what the library's Discord dashboard talks to.
//!
//! Management-token authed like the rest of `/v1/mgmt`. Tokens never appear in any response; a bot
//! is described by its application id, name, status and settings. Without the `discord` cargo
//! feature the one GET answers `501` so the dashboard can say the build has no bot support.

use axum::Router;

use crate::http::AppState;

#[cfg(feature = "discord")]
mod imp {
    use axum::extract::{Path, State};
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    use axum::routing::put;
    use base64::Engine;

    use crate::api::v1::mgmt::require_mgmt_auth;
    use crate::discord::emoji;
    use crate::discord::identity::Identity;
    use crate::discord::settings::{
        self, BotSettings, BotSettingsPatch, GuildSettings, GuildSettingsPatch,
    };
    use crate::discord::theme::{self, ThemeStatus};
    use crate::error::{AppError, AppResult};
    use crate::http::AppState;
    use serenity::all::GuildId;

    pub fn router() -> Router<AppState> {
        Router::new()
            .route("/mgmt/discord", get(overview))
            .route("/mgmt/discord/bots/{app_id}/settings", put(set_settings))
            .route("/mgmt/discord/bots/{app_id}/emojis", post(set_colour))
            .route("/mgmt/discord/bots/{app_id}/avatar", post(set_avatar))
            .route("/mgmt/discord/bots/{app_id}/restart", post(restart))
            .route(
                "/mgmt/discord/bots/{app_id}/guilds/{guild_id}/settings",
                put(set_guild_settings),
            )
            .route(
                "/mgmt/discord/bots/{app_id}/guilds/{guild_id}/leave",
                post(leave_guild),
            )
    }

    #[derive(Serialize)]
    struct NowPlaying {
        title: String,
        artist: String,
        album: Option<String>,
        position_ms: u64,
        duration_ms: u64,
        paused: bool,
        requested_by: String,
    }

    #[derive(Serialize)]
    struct VoiceChannel {
        id: String,
        name: Option<String>,
    }

    /// One guild the bot is in: live state plus that guild's settings.
    #[derive(Serialize)]
    struct GuildOverview {
        guild_id: String,
        name: Option<String>,
        voice_channel: Option<VoiceChannel>,
        now_playing: Option<NowPlaying>,
        queue_len: usize,
        listeners: usize,
        settings: GuildSettings,
    }

    async fn guilds_of(identity: &Arc<Identity>) -> Vec<GuildOverview> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for player in identity.players() {
            let snap = player.snapshot().await;
            seen.insert(player.guild_id);
            out.push(GuildOverview {
                guild_id: player.guild_id.get().to_string(),
                name: identity.guild_name(player.guild_id),
                voice_channel: snap.voice_channel.map(|c| VoiceChannel {
                    id: c.get().to_string(),
                    name: identity.channel_name(player.guild_id, c),
                }),
                now_playing: snap.current.as_ref().map(|c| NowPlaying {
                    title: c.item.track.title.clone(),
                    artist: c.item.track.artist.clone(),
                    album: c.item.track.album.clone(),
                    position_ms: c.position_ms,
                    duration_ms: c.item.track.duration_ms.max(0) as u64,
                    paused: c.paused,
                    requested_by: c.item.requested_by.get().to_string(),
                }),
                queue_len: snap.queue.len(),
                listeners: snap.listeners,
                settings: player.settings().await,
            });
        }
        // Guilds the bot sits in without having played anything yet.
        let app_id = identity.app_id_sync().unwrap_or(0).to_string();
        for guild in identity.guild_ids() {
            if seen.contains(&guild) {
                continue;
            }
            let gs = settings::load_guild(&identity.state.db, &app_id, &guild.get().to_string())
                .await
                .unwrap_or_else(|_| GuildSettings::defaults(&app_id, &guild.get().to_string()));
            out.push(GuildOverview {
                guild_id: guild.get().to_string(),
                name: identity.guild_name(guild),
                voice_channel: None,
                now_playing: None,
                queue_len: 0,
                listeners: 0,
                settings: gs,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// `PUT /v1/mgmt/discord/bots/{app_id}/guilds/{guild_id}/settings`.
    async fn set_guild_settings(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path((app_id, guild_id)): Path<(String, String)>,
        Json(patch): Json<GuildSettingsPatch>,
    ) -> AppResult<Json<GuildSettings>> {
        require_mgmt_auth(&headers, &state).await?;
        let identity = find(&app_id)?;
        let guild = GuildId::new(
            guild_id
                .parse()
                .map_err(|_| AppError::BadRequest("guild_id must be a snowflake".into()))?,
        );
        let player = identity.player(guild).await;
        // 24/7 turned on from the dashboard keeps the channel the bot is in, if any.
        let voice = player.voice_channel().await;
        let turning_on = patch.always_on == Some(true);
        let updated = player
            .update_settings(|s| {
                patch.apply(s);
                if turning_on && s.always_on_channel_id.is_none() {
                    s.always_on_channel_id = voice.map(|c| c.get().to_string());
                }
            })
            .await;
        Ok(Json(updated))
    }

    /// `POST /v1/mgmt/discord/bots/{app_id}/guilds/{guild_id}/leave`.
    async fn leave_guild(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path((app_id, guild_id)): Path<(String, String)>,
    ) -> AppResult<axum::http::StatusCode> {
        require_mgmt_auth(&headers, &state).await?;
        let identity = find(&app_id)?;
        let guild = GuildId::new(
            guild_id
                .parse()
                .map_err(|_| AppError::BadRequest("guild_id must be a snowflake".into()))?,
        );
        if let Some(player) = identity.player_arc(guild) {
            player
                .leave(crate::discord::player::LeaveReason::Command)
                .await;
        }
        Ok(axum::http::StatusCode::ACCEPTED)
    }

    /// `POST /v1/mgmt/discord/bots/{app_id}/restart`: drop the gateway and reconnect.
    async fn restart(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(app_id): Path<String>,
    ) -> AppResult<axum::http::StatusCode> {
        require_mgmt_auth(&headers, &state).await?;
        let identity = find(&app_id)?;
        for player in identity.players() {
            player
                .leave(crate::discord::player::LeaveReason::Shutdown)
                .await;
        }
        identity.request_restart();
        Ok(axum::http::StatusCode::ACCEPTED)
    }

    #[derive(Serialize)]
    struct BotOverview {
        index: u8,
        app_id: Option<String>,
        name: String,
        avatar_url: Option<String>,
        status: &'static str,
        error: Option<String>,
        invite_url: Option<String>,
        /// How many of the bot's icons are live as application emojis.
        emoji_count: usize,
        /// Colour, avatar, and whether a change is waiting on a rate limit (with the warning to
        /// show and the time the controls unlock).
        theme: ThemeStatus,
        settings: BotSettings,
        guilds: Vec<GuildOverview>,
    }

    #[derive(Serialize)]
    struct Overview {
        enabled: bool,
        bots: Vec<BotOverview>,
    }

    async fn describe(identity: &Arc<Identity>) -> BotOverview {
        let profile = identity.profile();
        let status = identity.status();
        let guilds = guilds_of(identity).await;
        BotOverview {
            index: identity.index,
            app_id: profile.as_ref().map(|p| p.app_id.to_string()),
            name: identity.display_name_sync(),
            avatar_url: profile.as_ref().and_then(|p| p.avatar_url.clone()),
            status: status.as_str(),
            error: match &status {
                crate::discord::Status::Failed(e) => Some(e.clone()),
                _ => None,
            },
            invite_url: profile.as_ref().map(|p| Identity::invite_url(p.app_id)),
            emoji_count: identity.icons().len(),
            theme: theme::status(&identity.settings()),
            settings: identity.settings(),
            guilds,
        }
    }

    /// `GET /v1/mgmt/discord`: every configured bot and its state.
    async fn overview(
        State(state): State<AppState>,
        headers: HeaderMap,
    ) -> AppResult<Json<Overview>> {
        require_mgmt_auth(&headers, &state).await?;
        let mut bots = Vec::new();
        if let Some(rt) = crate::discord::runtime() {
            for i in &rt.identities {
                bots.push(describe(i).await);
            }
        }
        Ok(Json(Overview {
            enabled: state.config.discord.enabled(),
            bots,
        }))
    }

    fn find(app_id: &str) -> AppResult<Arc<Identity>> {
        crate::discord::runtime()
            .and_then(|rt| {
                rt.identities
                    .iter()
                    .find(|i| i.app_id_sync().map(|id| id.to_string()).as_deref() == Some(app_id))
                    .cloned()
            })
            .ok_or(AppError::NotFound)
    }

    /// Refuse while a rate limit is in force: the dashboard disables the controls, and the API
    /// says the same thing to anything that did not.
    fn ensure_unlocked(identity: &Identity) -> AppResult<()> {
        let st = theme::status(&identity.settings());
        match st.retry_at {
            Some(at) => Err(AppError::BadRequest(format!(
                "{} (locked until {at})",
                st.warning
                    .unwrap_or_else(|| "a theme change is still waiting on Discord".into())
            ))),
            None => Ok(()),
        }
    }

    #[derive(Deserialize)]
    struct ColourRequest {
        /// `#rrggbb` (or `rgb`, with or without `#`). Absent: back to the default pink.
        hex: Option<String>,
    }

    /// `POST /v1/mgmt/discord/bots/{app_id}/emojis`: pick the bot's colour. The icon set, the
    /// container accent and (unless the owner brought their own) the avatar follow. Applied at
    /// once when Discord allows; queued with a warning when it does not.
    async fn set_colour(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(app_id): Path<String>,
        Json(body): Json<ColourRequest>,
    ) -> AppResult<Json<ThemeStatus>> {
        require_mgmt_auth(&headers, &state).await?;
        let identity = find(&app_id)?;
        ensure_unlocked(&identity)?;
        let hex = match body.hex.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(h) => emoji::normalize_hex(h)
                .ok_or_else(|| AppError::BadRequest(format!("{h:?} is not a hex colour")))?,
            None => emoji::DEFAULT_HEX.to_string(),
        };
        Ok(Json(theme::request_hex(&identity, &hex).await))
    }

    #[derive(Deserialize)]
    struct AvatarRequest {
        /// A `data:image/...;base64,...` URL of the owner's image (PNG, JPEG, GIF or WebP, up to
        /// Discord's 10 MB). Uploading one turns the managed mark off.
        image: String,
    }

    /// `POST /v1/mgmt/discord/bots/{app_id}/avatar`: the owner's own avatar.
    async fn set_avatar(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(app_id): Path<String>,
        Json(body): Json<AvatarRequest>,
    ) -> AppResult<Json<ThemeStatus>> {
        require_mgmt_auth(&headers, &state).await?;
        let identity = find(&app_id)?;
        ensure_unlocked(&identity)?;
        let (mime, b64) = body
            .image
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
            .ok_or_else(|| AppError::BadRequest("expected a data:image/...;base64 URL".into()))?;
        let ext = match mime {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            other => {
                return Err(AppError::BadRequest(format!(
                    "unsupported image type {other}"
                )))
            }
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| AppError::BadRequest(format!("bad base64: {e}")))?;
        if bytes.len() > 10 * 1024 * 1024 {
            return Err(AppError::BadRequest("image is larger than 10 MB".into()));
        }
        let status = theme::request_custom_avatar(&identity, ext, &bytes)
            .await
            .map_err(AppError::Internal)?;
        Ok(Json(status))
    }

    /// `PUT /v1/mgmt/discord/bots/{app_id}/settings`: any of the bot's runtime settings. Turning
    /// `avatar_managed` back on re-applies the mark.
    async fn set_settings(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(app_id): Path<String>,
        Json(patch): Json<BotSettingsPatch>,
    ) -> AppResult<Json<BotOverview>> {
        require_mgmt_auth(&headers, &state).await?;
        let identity = find(&app_id)?;
        let theme_change = patch.emoji_hex.is_some() || patch.avatar_managed.is_some();
        if theme_change {
            ensure_unlocked(&identity)?;
        }
        let mut settings = identity.settings();
        patch.apply(&mut settings);
        identity.set_settings(settings.clone());
        identity.save_settings().await;
        if theme_change {
            theme::tick(&identity).await;
        }
        crate::discord::presence::update(&identity).await;
        // A tightened allow-list takes effect now, not at the next join.
        if settings.allowed_guilds.is_some() {
            if let Some(http) = identity.http() {
                for guild in identity.guild_ids() {
                    if !settings.allows_guild(guild.get()) {
                        if let Some(player) = identity.player_arc(guild) {
                            player
                                .leave(crate::discord::player::LeaveReason::Command)
                                .await;
                        }
                        if let Err(e) = guild.leave(&http).await {
                            tracing::warn!(error = %e, guild = guild.get(), "leaving disallowed guild");
                        }
                    }
                }
            }
        }
        Ok(Json(describe(&identity).await))
    }
}

#[cfg(not(feature = "discord"))]
mod imp {
    use axum::routing::get;
    use axum::Router;

    use crate::error::{AppError, AppResult};
    use crate::http::AppState;

    pub fn router() -> Router<AppState> {
        Router::new().route("/mgmt/discord", get(unsupported))
    }

    async fn unsupported() -> AppResult<()> {
        Err(AppError::NotImplemented)
    }
}

pub fn router() -> Router<AppState> {
    imp::router()
}
