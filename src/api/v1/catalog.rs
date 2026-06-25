//! Catalog browsing endpoints.
//!
//! Browsing (metadata) is open - the library is self-hosted and behind the user's own firewall.
//! Only the stream endpoint gates on a capability token, since that's where audio bytes leave.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chordia_contracts::catalog::Track;
use serde::Deserialize;

use crate::catalog;
use crate::error::AppResult;
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

/// `GET /v1/libraries` - logical libraries hosted here.
async fn list_libraries(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let rows = catalog::list_libraries(&state.db).await?;
    let out: Vec<_> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "name": r.name,
                "path": r.path,
                "track_count": r.track_count,
            })
        })
        .collect();
    Ok(Json(serde_json::json!(out)))
}

/// `GET /v1/libraries/{library_id}/tracks`
async fn list_tracks(
    State(state): State<AppState>,
    Path(library_id): Path<String>,
    Query(page): Query<Pagination>,
) -> AppResult<Json<Vec<Track>>> {
    let tracks = catalog::list_tracks(&state.db, &library_id, page.limit, page.offset).await?;
    Ok(Json(tracks))
}

/// `GET /v1/tracks/{track_id}`
async fn get_track(
    State(state): State<AppState>,
    Path(track_id): Path<String>,
) -> AppResult<Json<Track>> {
    let track = catalog::get_track(&state.db, &track_id)
        .await?
        .ok_or(crate::error::AppError::NotFound)?;
    Ok(Json(track))
}
