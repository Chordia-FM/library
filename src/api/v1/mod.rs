//! Version 1 of the library HTTP API.

mod catalog;
mod health;
mod matcher;
pub(crate) mod mgmt;
mod pairing;
mod relay;
mod scrobbles;
mod stream;

use axum::Router;

use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(catalog::router())
        .merge(stream::router())
        .merge(matcher::router())
        .merge(relay::router())
        .merge(pairing::router())
        .merge(health::router())
        .merge(mgmt::router())
        .merge(scrobbles::router())
}
