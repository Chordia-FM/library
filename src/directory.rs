//! Directory heartbeat loop.
//!
//! Spawns a background task that POSTs the server's endpoint + TLS fingerprint to the Hub on the
//! interval the Hub recommends (defaulting to 30 s).  Authenticated with the server API key from
//! the pairing credentials - no user password involved.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::pairing::{HubClient, PairingCredentials};

/// `tls_fingerprint` is the SHA-256 of the in-process TLS leaf cert when TLS termination is
/// enabled, or empty when the server is plain-HTTP / edge-terminated (peers then validate the
/// connection normally rather than pinning).
pub fn start_heartbeat(
    hub: Arc<HubClient>,
    config: Arc<Config>,
    credentials: Arc<RwLock<Option<PairingCredentials>>>,
    tls_fingerprint: String,
) {
    tokio::spawn(async move {
        // Default to localhost for local dev; override with hub_endpoint in config for
        // production (set to the machine's LAN/public IP so peers can reach this server).
        let endpoint = config.hub_endpoint.clone().unwrap_or_else(|| {
            let ep = format!("http://localhost:{}", config.bind_port);
            info!(endpoint = %ep, "hub_endpoint not configured - using localhost for dev");
            ep
        });

        // Fire the first heartbeat after 1 s so the Hub marks this server online immediately
        // after pairing, rather than waiting the full 30 s interval.
        let mut interval_secs = 1u64;
        loop {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;

            let (server_id, api_key) = {
                let lock = credentials.read().await;
                match lock.as_ref() {
                    Some(c) => (c.server_id, c.server_api_key.clone()),
                    None => {
                        warn!("credentials cleared - stopping heartbeat");
                        return;
                    }
                }
            };

            match hub
                .heartbeat(server_id, &api_key, &endpoint, &tls_fingerprint)
                .await
            {
                Ok(next) => {
                    info!(server_id = %server_id, next_secs = next, "heartbeat ok");
                    interval_secs = next.max(10) as u64;
                }
                Err(e) => {
                    error!(error = %e, "heartbeat failed - retrying in 60 s");
                    interval_secs = 60;
                }
            }
        }
    });
}
