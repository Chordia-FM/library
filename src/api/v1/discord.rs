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

    use crate::api::v1::mgmt::require_mgmt_auth;
    use crate::discord::emoji;
    use crate::discord::identity::Identity;
    use crate::discord::settings::BotSettings;
    use crate::error::{AppError, AppResult};
    use crate::http::AppState;

    pub fn router() -> Router<AppState> {
        Router::new()
            .route("/mgmt/discord", get(overview))
            .route("/mgmt/discord/bots/{app_id}/emojis", post(set_emojis))
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

    #[derive(Deserialize)]
    struct EmojiRequest {
        /// `#rrggbb` (or `rgb`, with or without `#`). Absent: back to the default pink.
        hex: Option<String>,
    }

    #[derive(Serialize)]
    struct EmojiResponse {
        hex: String,
        /// Icons now live on Discord. Zero when the bot is offline: the colour is saved and the set
        /// is generated on its next connection.
        count: usize,
        applied: bool,
    }

    /// `POST /v1/mgmt/discord/bots/{app_id}/emojis`: pick the icon colour and regenerate the
    /// application emoji set in it.
    async fn set_emojis(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(app_id): Path<String>,
        Json(body): Json<EmojiRequest>,
    ) -> AppResult<Json<EmojiResponse>> {
        require_mgmt_auth(&headers, &state).await?;
        let identity = find(&app_id)?;
        let hex = match body.hex.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(h) => emoji::normalize_hex(h)
                .ok_or_else(|| AppError::BadRequest(format!("{h:?} is not a hex colour")))?,
            None => emoji::DEFAULT_HEX.to_string(),
        };
        let mut settings = identity.settings();
        settings.emoji_hex = Some(hex.clone());
        identity.set_settings(settings.clone());
        identity.save_settings().await;

        let Some(http) = identity.http() else {
            return Ok(Json(EmojiResponse {
                hex,
                count: 0,
                applied: false,
            }));
        };
        let replace = settings.emoji_hex_applied.as_deref() != Some(hex.as_str());
        let set = emoji::provision(&http, &hex, replace)
            .await
            .map_err(|e| AppError::BadGateway(format!("Discord refused the emoji upload: {e}")))?;
        let count = set.len();
        identity.set_icons(Arc::new(set));
        settings.emoji_hex_applied = Some(hex.clone());
        identity.set_settings(settings);
        identity.save_settings().await;
        Ok(Json(EmojiResponse {
            hex,
            count,
            applied: true,
        }))
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
