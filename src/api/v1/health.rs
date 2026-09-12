//! Liveness/readiness for the library API surface.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::auth::EmbeddedSession;
use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/ping", get(ping))
}

/// `GET /v1/ping` - readiness probe.
///
/// Open on a normal server, because the pairing wizard probes it before any credential exists.
/// Behind the local session when this library is embedded in the desktop app, where the body would
/// otherwise let any page the user visits find the loopback port. See [`EmbeddedSession`].
async fn ping(State(state): State<AppState>, _session: EmbeddedSession) -> Json<serde_json::Value> {
    let paired = state.credentials.read().await.is_some();
    let library_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM libraries")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    Json(json!({
        "status": "ok",
        "paired": paired,
        "libraries": library_count,
    }))
}
