//! Liveness/readiness for the library API surface.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/ping", get(ping))
}

/// `GET /v1/ping` - readiness probe.
async fn ping(State(state): State<AppState>) -> Json<serde_json::Value> {
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
