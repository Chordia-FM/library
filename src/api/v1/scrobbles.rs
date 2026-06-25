//! Owner-side scrobble ingest (Phase A3).
//!
//! `POST /v1/scrobbles` lets an owner's client hand its **own** library a batch of listening events
//! to buffer and forward to the Hub. This is the Hub-unreachable fallback: the client can always
//! reach its own library directly, and the library's reporter forwards when the Hub returns.
//!
//! Authenticated with the **management token** (`Authorization: Library {management_token}`), which
//! is owner-scoped - so the Hub can safely attribute the forwarded events to the server's owner.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use chordia_contracts::scrobble::ScrobbleBatch;

use crate::api::v1::mgmt::require_mgmt_auth;
use crate::error::AppResult;
use crate::http::AppState;
use crate::scrobble;

pub fn router() -> Router<AppState> {
    Router::new().route("/scrobbles", post(ingest))
}

/// `POST /v1/scrobbles` - durably enqueue the owner's listening events for forwarding to the Hub.
async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ScrobbleBatch>,
) -> AppResult<StatusCode> {
    require_mgmt_auth(&headers, &state).await?;
    for event in &body.events {
        scrobble::enqueue(&state.db, event).await?;
    }
    Ok(StatusCode::ACCEPTED)
}
