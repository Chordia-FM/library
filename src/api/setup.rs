//! One-time browser setup redirect.
//!
//! When the library starts unpaired, it generates a random token and prints:
//!   "Visit http://localhost:8443/setup/{token} to get started."
//!
//! `GET /setup/{token}` validates the token and redirects the browser to the frontend's
//! `/library/setup` page, passing the library URL and token as query params so the frontend
//! can drive the rest of the pairing flow.

use axum::extract::{Path, State};
use axum::response::Redirect;

use crate::error::AppError;
use crate::http::AppState;

pub async fn setup_redirect(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Redirect, AppError> {
    let expected = state.setup_token.read().await.clone();
    match expected {
        Some(ref t) if t == &token => {
            let port = state.config.bind_port;
            // Prefer the public endpoint (advertised to the Hub) so remote/VPS pairing works from
            // any browser; fall back to localhost for a same-machine setup.
            let library_url = state
                .config
                .hub_endpoint
                .clone()
                .map(|ep| ep.trim_end_matches('/').to_string())
                .unwrap_or_else(|| format!("http://localhost:{port}"));
            let frontend = &state.config.frontend_url;
            let target =
                format!("{frontend}/library/setup?library_url={library_url}&token={token}");
            Ok(Redirect::to(&target))
        }
        _ => Err(AppError::NotFound),
    }
}
