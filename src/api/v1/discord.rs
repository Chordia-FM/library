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
    use crate::discord::settings::{BotSettings, BotSettingsPatch};
    use crate::discord::theme::{self, ThemeStatus};
    use crate::error::{AppError, AppResult};
    use crate::http::AppState;

    pub fn router() -> Router<AppState> {
        Router::new()
            .route("/mgmt/discord", get(overview))
            .route("/mgmt/discord/bots/{app_id}/settings", put(set_settings))
            .route("/mgmt/discord/bots/{app_id}/emojis", post(set_colour))
            .route("/mgmt/discord/bots/{app_id}/avatar", post(set_avatar))
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
    }

    #[derive(Serialize)]
    struct Overview {
        enabled: bool,
        bots: Vec<BotOverview>,
    }

    fn describe(identity: &Identity) -> BotOverview {
        let profile = identity.profile();
        let status = identity.status();
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
        }
    }

    /// `GET /v1/mgmt/discord`: every configured bot and its state.
    async fn overview(
        State(state): State<AppState>,
        headers: HeaderMap,
    ) -> AppResult<Json<Overview>> {
        require_mgmt_auth(&headers, &state).await?;
        let bots = match crate::discord::runtime() {
            Some(rt) => rt.identities.iter().map(|i| describe(i)).collect(),
            None => Vec::new(),
        };
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
        identity.set_settings(settings);
        identity.save_settings().await;
        if theme_change {
            theme::tick(&identity).await;
        }
        crate::discord::presence::update(&identity).await;
        Ok(Json(describe(&identity)))
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
