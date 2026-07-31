//! Catalog browsing endpoints.
//!
//! These used to be open, on the reasoning that a self-hosted library sits behind the owner's
//! firewall. It does not: the whole point of the data plane is that clients reach the library
//! *directly* over the internet (`library.chordia.dev` in the reference deployment), so "open"
//! meant anyone who learned the hostname could enumerate the entire catalog — and `list_libraries`
//! additionally handed out the server's absolute filesystem paths.
//!
//! They now require the same capability token `stream` does, scoped to the library being read.
//! `LibraryClient` already sent that token on every one of these calls, so this closes the hole
//! without any client change.
//!
//! Filesystem paths are no longer returned here at all. `GET /v1/mgmt/libraries`, behind the
//! management token, is the only surface that needs them and already provides them.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chordia_contracts::auth::CapabilityAction;
use chordia_contracts::catalog::Track;
use serde::Deserialize;

use crate::auth::{require_action, CapToken};
use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::http::AppState;

#[derive(Deserialize)]
pub struct Pagination {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}
fn default_limit() -> i64 {
    200
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/libraries", get(list_libraries))
        .route("/libraries/{library_id}/tracks", get(list_tracks))
        .route("/tracks/{track_id}", get(get_track))
}

/// Assert the token was minted for the library that owns this local row.
async fn scope_local_library(
    db: &sqlx::SqlitePool,
    local_library_id: &str,
    hub_library_id: &str,
) -> AppResult<()> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM libraries WHERE id = ? AND hub_library_id = ?")
            .bind(local_library_id)
            .bind(hub_library_id)
            .fetch_one(db)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
    if count == 0 {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Assert the token was minted for a library that contains this track.
async fn scope_track(db: &sqlx::SqlitePool, track_id: &str, hub_library_id: &str) -> AppResult<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_tracks lt \
         JOIN libraries l ON l.id = lt.library_id \
         WHERE lt.track_id = ? AND l.hub_library_id = ?",
    )
    .bind(track_id)
    .bind(hub_library_id)
    .fetch_one(db)
    .await
    .map_err(|e| AppError::Internal(e.into()))?;
    if count == 0 {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// `GET /v1/libraries` - the library this token authorizes, without its filesystem path.
async fn list_libraries(
    State(state): State<AppState>,
    token: CapToken,
) -> AppResult<Json<serde_json::Value>> {
    let claims = require_action(&token, CapabilityAction::StreamRead)?;
    let rows = catalog::list_libraries_for_hub(&state.db, &claims.library_id.to_string()).await?;
    let out: Vec<_> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "name": r.name,
                "track_count": r.track_count,
            })
        })
        .collect();
    Ok(Json(serde_json::json!(out)))
}

/// `GET /v1/libraries/{library_id}/tracks`
async fn list_tracks(
    State(state): State<AppState>,
    token: CapToken,
    Path(library_id): Path<String>,
    Query(page): Query<Pagination>,
) -> AppResult<Json<Vec<Track>>> {
    let claims = require_action(&token, CapabilityAction::StreamRead)?;
    scope_local_library(&state.db, &library_id, &claims.library_id.to_string()).await?;
    let tracks = catalog::list_tracks(&state.db, &library_id, page.limit, page.offset).await?;
    Ok(Json(tracks))
}

/// `GET /v1/tracks/{track_id}`
async fn get_track(
    State(state): State<AppState>,
    token: CapToken,
    Path(track_id): Path<String>,
) -> AppResult<Json<Track>> {
    let claims = require_action(&token, CapabilityAction::StreamRead)?;
    scope_track(&state.db, &track_id, &claims.library_id.to_string()).await?;
    let track = catalog::get_track(&state.db, &track_id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(track))
}
