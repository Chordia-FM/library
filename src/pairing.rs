//! Hub API client - pairing and heartbeat.
//!
//! Pairing: the library forwards a logged-in user's Bearer token to the Hub, which assigns a
//! `server_id` and issues a `server_api_key`.  The library persists those in `data/pairing.json`.
//!
//! Heartbeat: authenticated with `Authorization: Library {server_api_key}` so no user credentials
//! are ever stored on the library server.

use std::path::Path;

use chordia_contracts::catalog::{CatalogPruneRequest, CatalogSyncRequest, CatalogSyncResponse};
use chordia_contracts::directory::{HeartbeatRequest, HeartbeatResponse};
use chordia_contracts::scrobble::ScrobbleBatch;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Credentials obtained during pairing - persisted across restarts in `data/pairing.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCredentials {
    pub server_id: Uuid,
    /// API key for Hub heartbeat auth (`Authorization: Library {server_api_key}`).
    pub server_api_key: String,
    /// Token the library issues to the frontend for management API calls (add/remove folders).
    pub management_token: String,
}

impl PairingCredentials {
    pub fn load(data_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(data_dir.join("pairing.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(data_dir.join("pairing.json"), text)?;
        Ok(())
    }
}

/// Response from `POST /v1/libraries/pair` on the Hub.
#[derive(Debug, Deserialize)]
pub struct HubPairResponse {
    pub server_id: Uuid,
    pub server_api_key: String,
}

/// Minimal Hub client - no stored credentials.
pub struct HubClient {
    base_url: String,
    http: reqwest::Client,
}

impl HubClient {
    pub fn new(base_url: String, http: reqwest::Client) -> Self {
        Self { base_url, http }
    }

    /// Call `POST /v1/libraries/pair` forwarding the user's access token.
    /// Returns the Hub-assigned server credentials.
    pub async fn pair(&self, user_access_token: &str) -> anyhow::Result<HubPairResponse> {
        let url = format!("{}/v1/libraries/pair", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(user_access_token)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Hub pair failed {status}: {body}");
        }
        Ok(resp.json().await?)
    }

    /// Call `POST /v1/directory/heartbeat` using the server's own API key.
    pub async fn heartbeat(
        &self,
        server_id: Uuid,
        server_api_key: &str,
        endpoint: &str,
        tls_fingerprint: &str,
    ) -> anyhow::Result<u32> {
        let url = format!("{}/v1/directory/heartbeat", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .json(&HeartbeatRequest {
                server_id,
                endpoint: endpoint.to_string(),
                tls_fingerprint: tls_fingerprint.to_string(),
            })
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("heartbeat failed {}", resp.status());
        }
        Ok(resp.json::<HeartbeatResponse>().await?.next_interval_secs)
    }

    /// Forward buffered listening events to the Hub on the owner's behalf
    /// (`POST /v1/scrobbles:ingest`, server-API-key authed). The Hub dedupes on `event_id`.
    pub async fn forward_scrobbles(
        &self,
        server_api_key: &str,
        batch: &ScrobbleBatch,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v1/scrobbles:ingest", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .json(batch)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("scrobble forward failed {status}: {body}");
        }
        Ok(())
    }

    /// Push a batch of catalog tracks to the Hub. Returns the cover hashes the Hub still needs.
    pub async fn sync_catalog(
        &self,
        server_api_key: &str,
        req: &CatalogSyncRequest,
    ) -> anyhow::Result<CatalogSyncResponse> {
        let url = format!("{}/v1/catalog/sync", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("catalog sync failed {status}: {body}");
        }
        Ok(resp.json().await?)
    }

    /// Report the authoritative set of track refs so the Hub drops memberships for deleted files.
    pub async fn prune_catalog(
        &self,
        server_api_key: &str,
        req: &CatalogPruneRequest,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v1/catalog/prune", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("catalog prune failed {status}: {body}");
        }
        Ok(())
    }

    /// Upload embedded cover bytes the Hub was missing.
    pub async fn upload_cover(
        &self,
        server_api_key: &str,
        hash: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v1/catalog/covers/{hash}", self.base_url);
        let resp = self
            .http
            .put(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .header("Content-Type", mime)
            .body(bytes)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("cover upload failed {}", resp.status());
        }
        Ok(())
    }
}
