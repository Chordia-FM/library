//! HTTP layer: shared state, router assembly, middleware.

pub mod middleware;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::auth::JwksCache;
use crate::config::Config;
use crate::pairing::PairingCredentials;
use crate::transcode::Transcoder;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<Config>,
    pub jwks: Arc<JwksCache>,
    pub http: reqwest::Client,
    /// Server credentials from `data/pairing.json`. None until paired.
    pub credentials: Arc<RwLock<Option<PairingCredentials>>>,
    /// One-time setup token printed to the terminal. Cleared after the claim flow completes.
    pub setup_token: Arc<RwLock<Option<String>>>,
    /// On-the-fly transcoder for the lower quality tiers (shared ffmpeg concurrency and cache).
    pub transcoder: Arc<Transcoder>,
    /// SHA-256 of the in-process TLS leaf cert advertised to the Hub, or empty when plain-HTTP or
    /// edge-terminated. Computed once at startup, then shared by the heartbeat and pairing-claim flows.
    pub tls_fingerprint: String,
}

impl AppState {
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        // Sandboxed music storage. The browse API is locked to this subtree.
        std::fs::create_dir_all(config.data_dir.join("music"))?;
        // Disk cache for transcoded (lower-tier) audio.
        let transcode_cache_dir = config.transcode_cache_dir();
        std::fs::create_dir_all(&transcode_cache_dir)?;
        let opts = SqliteConnectOptions::new()
            .filename(config.data_dir.join("library.sqlite"))
            .create_if_missing(true);
        let db = SqlitePool::connect_with(opts).await?;
        sqlx::migrate!("./migrations").run(&db).await?;

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let jwks = JwksCache::new(config.backend_url.clone(), http.clone());

        let credentials = PairingCredentials::load(&config.data_dir);

        let transcoder = Arc::new(Transcoder::new(&config.transcode, transcode_cache_dir));

        // Compute the TLS leaf fingerprint once if in-process TLS is configured; empty otherwise.
        let tls_fingerprint = match config.tls_paths() {
            Some((cert, _)) => crate::tls::leaf_fingerprint_from_pem(&cert)?,
            None => String::new(),
        };

        Ok(Self {
            db,
            config: Arc::new(config.clone()),
            jwks,
            http,
            credentials: Arc::new(RwLock::new(credentials)),
            setup_token: Arc::new(RwLock::new(None)),
            transcoder,
            tls_fingerprint,
        })
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        // One-time browser setup link (printed to terminal on first run).
        .route("/setup/{token}", get(crate::api::setup::setup_redirect))
        .nest("/v1", crate::api::v1::router())
        // Browser clients call the catalog/match endpoints cross-origin with an `Authorization`
        // header. `CorsLayer::permissive()` sends `Allow-Headers: *`, which the Fetch spec does not
        // treat as covering `Authorization`, so we mirror the requested headers instead (which does
        // cover it), while keeping origin/methods open and exposing all headers for Range streaming.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(AllowHeaders::mirror_request())
                .expose_headers(Any),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
