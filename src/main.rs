//! Chordia self-hosted library server entrypoint.
//!
//! Boot sequence:
//!  1. Load config from TOML.
//!  2. Init telemetry.
//!  3. Open SQLite + run migrations.
//!  4. Register any libraries from config and kick off initial scans.
//!  5. If already paired (data/pairing.json exists): start heartbeat.
//!     If NOT paired: generate a one-time setup token and print the setup URL.
//!  6. Bind and serve.

#![allow(dead_code)]

mod api;
mod auth;
mod catalog;
mod catalog_sync;
mod config;
mod dedupe;
mod directory;
mod error;
mod fingerprint;
mod http;
mod index;
mod loudness;
mod metadata;
mod organize;
mod pairing;
mod playback;
mod relay;
mod scanner;
mod scrobble;
mod streaming;
mod telemetry;
mod tls;
mod transcode;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use rand::distributions::Alphanumeric;
use rand::Rng;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config::Config::load().context("loading configuration")?;
    telemetry::init(&config);

    // Install the rustls crypto provider once, before any TLS config is built.
    tls::install_crypto_provider();

    let state = http::AppState::new(&config)
        .await
        .context("initialising application state")?;

    // Register libraries from config and start scans.
    let mut watcher_libs: Vec<(String, std::path::PathBuf)> = Vec::new();

    // Config no longer has a `libraries` field, since library folders are managed via the
    // setup UI and stored in SQLite. We do a catch-all scan of any libraries already in the DB
    // (from a previous run) so they stay up to date after a restart.
    let existing_libs: Vec<(String, String)> = sqlx::query_as("SELECT id, path FROM libraries")
        .fetch_all(&state.db)
        .await
        .context("loading libraries from DB")?;

    for (lib_id, path_str) in existing_libs {
        let path = std::path::PathBuf::from(&path_str);
        let db = state.db.clone();
        let id = lib_id.clone();
        let scan_path = path.clone();
        tokio::spawn(async move {
            scanner::initial_scan(&db, &id, &scan_path).await;
            // Drop entries for files deleted while the server was down (cheap stat-only pass).
            scanner::prune_missing(&db, &id).await;
        });
        watcher_libs.push((lib_id, path));
    }

    if !watcher_libs.is_empty() {
        scanner::start_watcher(state.db.clone(), watcher_libs.clone());
        // Periodic rescan/prune backstop for changes the live watcher missed (disabled at 0).
        if config.scan.interval_minutes > 0 {
            scanner::start_scheduler(
                state.db.clone(),
                watcher_libs,
                std::time::Duration::from_secs(config.scan.interval_minutes * 60),
            );
        }
    }

    // Heartbeat or setup.
    if state.credentials.read().await.is_some() {
        let hub = Arc::new(pairing::HubClient::new(
            config.backend_url.clone(),
            state.http.clone(),
        ));
        directory::start_heartbeat(
            hub,
            state.config.clone(),
            state.credentials.clone(),
            state.tls_fingerprint.clone(),
        );
    } else {
        // Not yet paired, so generate a one-time setup token and print the URL.
        let token: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        *state.setup_token.write().await = Some(token.clone());

        let port = config.bind_port;
        // Match the scheme the server is actually serving on so the browser link works directly.
        let scheme = if config.tls_paths().is_some() {
            "https"
        } else {
            "http"
        };
        let setup_url = format!("{scheme}://localhost:{port}/setup/{token}");

        // Box width is driven by the URL line (always the longest).
        // Interior = url_len + 4 (two spaces padding on each side).
        let inner = setup_url.len() + 4;
        let bar: String = "═".repeat(inner);
        let blank: String = " ".repeat(inner);

        let padded = |text: &str| -> String {
            let spaces = inner.saturating_sub(2 + text.len());
            format!("  {text}{}", " ".repeat(spaces))
        };

        // Write to stdout so the URL ends up in the stdout log when running detached
        // (both tracing and the banner share the same stream that way).
        println!();
        println!("  ╔{bar}╗");
        println!(
            "  ║{}║",
            padded("Chordia Library is not yet paired with a Hub account.")
        );
        println!(
            "  ║{}║",
            padded("Open the link below in your browser to complete setup:")
        );
        println!("  ║{blank}║");
        println!("  ║  {setup_url}  ║");
        println!("  ║{blank}║");
        println!(
            "  ║{}║",
            padded("This link is single-use. Keep it private.")
        );
        println!("  ╚{bar}╝");
        println!();
        // Emit the URL through tracing as well so it's discoverable in structured log pipelines
        // and is guaranteed to be flushed to the log file before the server starts accepting requests.
        tracing::info!(
            setup_url,
            "library not paired, open the setup URL to complete pairing"
        );
    }

    // Catalog sync to the Hub (no-op unless paired and metadata_storage = hub).
    catalog_sync::start_sync_loop(state.clone());

    // Scrobble reporter: forward the owner's buffered listening events to the Hub.
    scrobble::start_reporter(state.clone());

    // AcoustID identification (no-op unless [acoustid] api_key is configured).
    fingerprint::start_identification(state.clone());

    // Loudness analysis (EBU R128 to ReplayGain; on unless [loudness] enabled = false).
    loudness::start_analysis(state.clone());

    // Re-upload dedupe (keep highest-quality copy; on unless [scan] dedupe_reuploads = false).
    dedupe::start_dedupe(state.clone());

    // Bind and serve.
    let app = http::router(state);
    let addr = SocketAddr::from(([0, 0, 0, 0], config.bind_port));

    if let Some((cert, key)) = config.tls_paths() {
        // In-process HTTPS: peers/native clients pin the leaf fingerprint advertised to the Hub.
        let tls_config = tls::rustls_config(&cert, &key)
            .await
            .context("loading TLS configuration")?;
        tracing::info!(port = config.bind_port, "chordia-library listening (HTTPS)");
        axum_server::bind_rustls(addr, tls_config)
            .serve(app.into_make_service())
            .await
            .context("TLS server error")?;
    } else {
        // Plain HTTP, so rely on edge TLS (tunnel or reverse proxy) for transport security.
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding port {}", config.bind_port))?;
        tracing::info!(port = config.bind_port, "chordia-library listening (HTTP)");
        axum::serve(listener, app).await.context("server error")?;
    }
    Ok(())
}
