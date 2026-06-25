//! Audio streaming endpoint - the data plane.
//!
//! The `CapToken` extractor accepts `Authorization: Bearer <token>` OR `?token=<token>` (for
//! HTML `<audio>` elements that cannot set custom headers).

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use chordia_contracts::auth::CapabilityAction;
use chordia_contracts::streaming::{QualityProfile, StreamQuery};

use crate::auth::{require_action, CapToken};
use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::http::AppState;
use crate::streaming;

/// Verify the capability token's `library_id` claim matches the library that owns `track_id`.
/// Libraries with no `hub_library_id` set yet (pre-M4 setup) are exempted so existing
/// installs keep working; once linked after a fresh setup they are enforced strictly.
async fn check_library_scope(
    db: &sqlx::SqlitePool,
    track_id: &str,
    hub_library_id: &str,
) -> AppResult<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_tracks lt \
         JOIN libraries l ON l.id = lt.library_id \
         WHERE lt.track_id = ? \
         AND (l.hub_library_id = ? OR l.hub_library_id IS NULL)",
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

pub fn router() -> Router<AppState> {
    Router::new().route("/stream/{track_id}", get(stream))
}

/// `GET /v1/stream/{track_id}?profile=original|high|normal|data_saver[&token=<cap_token>]`
///
/// Requires a `StreamRead` capability token. `Original` (the default) is byte-for-byte lossless
/// passthrough; lower tiers are transcoded on the fly (ffmpeg) and cached. Spatial/Atmos tracks
/// are always served as `Original` - they are passthrough-only and never transcoded.
async fn stream(
    State(state): State<AppState>,
    token: CapToken,
    Path(track_id): Path<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> AppResult<axum::response::Response> {
    require_action(&token, CapabilityAction::StreamRead)?;

    let meta = catalog::get_track_meta(&state.db, &track_id)
        .await?
        .ok_or(AppError::NotFound)?;

    check_library_scope(&state.db, &track_id, &token.claims.library_id.to_string()).await?;

    let source = std::path::Path::new(&meta.path);

    // Spatial/Atmos is passthrough-only; force the original bitstream regardless of the request.
    let want_transcode = q.profile != QualityProfile::Original && !meta.spatial;
    if want_transcode {
        if let Some(t) = state
            .transcoder
            .ensure(source, &meta.content_hash, q.profile)
            .await?
        {
            return streaming::serve_range(
                &t.path,
                &t.etag,
                t.content_codec,
                &headers,
                state.config.max_stream_kbps,
            )
            .await
            .map_err(AppError::Internal);
        }
    }

    // Original tier (or spatial passthrough).
    streaming::serve_range(
        source,
        &meta.content_hash,
        &meta.codec,
        &headers,
        state.config.max_stream_kbps,
    )
    .await
    .map_err(AppError::Internal)
}
