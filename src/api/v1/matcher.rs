//! Own-copy lookup endpoint.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chordia_contracts::catalog::{MatchQuery, MatchResult};

use crate::error::AppResult;
use crate::http::AppState;
use crate::playback;

pub fn router() -> Router<AppState> {
    Router::new().route("/tracks/match", get(match_track))
}

/// `GET /v1/tracks/match?content_hash=&acoustid=&recording_mbid=&artist_norm=&title_norm=&duration_ms=`
///
/// No capability token required - the caller is typically the owner's own client checking its
/// own library before requesting a relay token from the Hub.
async fn match_track(
    State(state): State<AppState>,
    Query(q): Query<MatchQuery>,
) -> AppResult<Json<MatchResult>> {
    let result = playback::match_track(&state.db, &q).await?;
    Ok(Json(result))
}
