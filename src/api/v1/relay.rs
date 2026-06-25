//! DJ relay endpoint. (Post-MVP)

use axum::routing::post;
use axum::Router;

use crate::error::AppError;
use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/relay", post(relay))
}

/// `POST /v1/relay` - the owner's client asks its own library to fetch a track it does not own
/// from the DJ's library and re-serve it (`RelayRequest`: dj endpoint + fingerprint + relay
/// token). The library pins the DJ's TLS fingerprint and authenticates with the relay token.
async fn relay() -> AppError {
    AppError::NotImplemented
}
