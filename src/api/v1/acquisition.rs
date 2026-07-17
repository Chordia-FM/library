//! Direct interactive acquisition endpoints (client → library), capability-authed with
//! `ManageAcquisition` (minted by the Hub only for library owners). A low-latency Prowlarr preview
//! that bypasses the Hub job queue; actual downloads still go through the queue (which monitors +
//! imports). The Hub never sees indexer secrets or torrents.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use chordia_contracts::acquisition::{AcquisitionSearch, CandidateInput};
use chordia_contracts::auth::CapabilityAction;
use serde_json::json;

use crate::acquisition::{quality, AcquisitionClient};
use crate::auth::{require_action, CapToken};
use crate::error::{AppError, AppResult};
use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/acquisition/search", post(search))
        .route("/acquisition/status", get(acq_status))
}

/// `POST /v1/acquisition/search`: live, scored Prowlarr search (no grab).
async fn search(
    State(state): State<AppState>,
    token: CapToken,
    Json(body): Json<AcquisitionSearch>,
) -> AppResult<Json<Vec<CandidateInput>>> {
    require_action(&token, CapabilityAction::ManageAcquisition)?;
    let client = AcquisitionClient::from_config(&state.config.acquisition)
        .ok_or_else(|| AppError::BadRequest("acquisition not configured".into()))?;
    let query = [Some(body.artist), body.album]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let mut releases = client
        .search(&query)
        .await
        .map_err(|e| AppError::BadGateway(e.to_string()))?;
    quality::rank(&mut releases, None);
    Ok(Json(
        releases
            .iter()
            .take(30)
            .map(|r| CandidateInput {
                guid: r.guid.clone(),
                title: r.title.clone(),
                indexer: r.indexer.clone(),
                quality_label: Some(quality::label_for(&r.title)),
                score: None,
                size_bytes: Some(r.size),
                seeders: Some(r.seeders),
                leechers: Some(r.leechers),
                // This response goes to the BROWSER: never leak indexer/Prowlarr URLs (they can
                // embed API keys). Sources are only persisted on the server-authed Hub path.
                download_url: None,
                magnet_url: None,
                info_hash: None,
            })
            .collect(),
    ))
}

/// `GET /v1/acquisition/status`: config + indexer health for the settings UI.
async fn acq_status(
    State(state): State<AppState>,
    token: CapToken,
) -> AppResult<Json<serde_json::Value>> {
    require_action(&token, CapabilityAction::ManageAcquisition)?;
    let cfg = &state.config.acquisition;
    let (configured, indexer_count) = match AcquisitionClient::from_config(cfg) {
        Some(c) => (true, c.indexer_count().await.unwrap_or(0)),
        None => (false, 0),
    };
    Ok(Json(json!({
        "enabled": cfg.enabled,
        "configured": configured,
        "indexer_count": indexer_count,
        "client_kind": "qbittorrent",
    })))
}
