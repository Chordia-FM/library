//! Pairing endpoint - browser-initiated setup flow. (M2)
//!
//! The user visits the one-time URL printed at startup, which redirects their browser to the
//! frontend `/library/setup` page.  The frontend then calls `POST /v1/pairing/claim` with:
//!   - `Authorization: Bearer {hub_access_token}` (the logged-in user's Hub JWT)
//!   - `X-Setup-Token: {token}`                   (from the URL the user visited)
//!
//! The library forwards the Hub JWT to `POST /v1/libraries/pair`, receives a `server_id` and
//! `server_api_key`, saves them to `data/pairing.json`, and returns a `management_token` the
//! frontend uses for subsequent library-folder management calls.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::Serialize;
use uuid::Uuid;

use crate::directory;
use crate::error::{AppError, AppResult};
use crate::http::AppState;
use crate::pairing::{HubClient, PairingCredentials};

#[derive(Serialize)]
struct ClaimResponse {
    server_id: Uuid,
    /// Token the frontend uses for management API calls on this library server.
    management_token: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/pairing/claim", post(claim))
}

async fn claim(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<(StatusCode, Json<ClaimResponse>)> {
    // Validate setup token.
    let provided_token = headers
        .get("X-Setup-Token")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("X-Setup-Token header required".into()))?;

    {
        let lock = state.setup_token.read().await;
        if lock.as_deref() != Some(provided_token) {
            return Err(AppError::Unauthorized);
        }
    }

    // Extract the user's Hub access token.
    let user_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AppError::Unauthorized)?
        .to_string();

    // Forward to Hub to allocate server_id + server_api_key.
    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());
    let pair = hub
        .pair(&user_token)
        .await
        .map_err(|e| AppError::BadGateway(e.to_string()))?;

    // Generate a management token for this library server.
    let management_token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();

    // Persist credentials.
    let creds = PairingCredentials {
        server_id: pair.server_id,
        server_api_key: pair.server_api_key,
        management_token: management_token.clone(),
    };
    creds
        .save(&state.config.data_dir)
        .map_err(AppError::Internal)?;

    *state.credentials.write().await = Some(creds.clone());

    // Invalidate the one-time setup token.
    *state.setup_token.write().await = None;

    // Start the heartbeat now that we have credentials.
    let hub_arc = Arc::new(HubClient::new(
        state.config.backend_url.clone(),
        state.http.clone(),
    ));
    directory::start_heartbeat(
        hub_arc,
        state.config.clone(),
        state.credentials.clone(),
        state.tls_fingerprint.clone(),
    );

    tracing::info!(server_id = %pair.server_id, "paired with Hub");

    Ok((
        StatusCode::CREATED,
        Json(ClaimResponse {
            server_id: pair.server_id,
            management_token,
        }),
    ))
}
