//! Own-copy lookup endpoint.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chordia_contracts::auth::CapabilityAction;
use chordia_contracts::catalog::{MatchQuery, MatchResult};

use crate::auth::{require_action, CapToken};
use crate::error::AppResult;
use crate::http::AppState;
use crate::playback;

pub fn router() -> Router<AppState> {
    Router::new().route("/tracks/match", get(match_track))
}

/// `GET /v1/tracks/match?content_hash=&acoustid=&recording_mbid=&artist_norm=&title_norm=&duration_ms=`
///
/// Requires the same `StreamRead` capability token the catalog and stream endpoints do — or, for an
/// embedded library, the local session standing in for one.
///
/// This used to take no credential at all, on the reasoning that the caller is the owner's own
/// client checking its own library. It is not only that: the route is served on the same public
/// endpoint as everything else, so anyone who learned the hostname (or any web page the owner
/// visited, given `Allow-Origin: *`) could ask "do you have this recording" over a dictionary and
/// read back the full record, file SHA-256 included.
///
/// A token alone is not enough either, because possession is exactly what a per-grantee share
/// exclusion withholds: the answer is scoped to what this token could actually stream, and a hit
/// the caller may not read is reported as no hit rather than as a 403 that confirms it anyway.
async fn match_track(
    State(state): State<AppState>,
    token: CapToken,
    Query(q): Query<MatchQuery>,
) -> AppResult<Json<MatchResult>> {
    let claims = require_action(&token, CapabilityAction::StreamRead)?;
    let result = playback::match_track(&state.db, &q).await?;

    // An embedded library has no Hub, so there is no narrower grant to check against — see
    // `auth::LocalSession`. Its one user owns every folder in it.
    if token.local {
        return Ok(Json(result));
    }

    if let Some(track) = &result.track {
        if !super::stream::readable_by(&state.db, claims, &track.id.to_string()).await? {
            return Ok(Json(MatchResult {
                track: None,
                matched_on: None,
            }));
        }
    }
    Ok(Json(result))
}
