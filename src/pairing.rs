//! Hub API client - pairing and heartbeat.
//!
//! Pairing: the library forwards a logged-in user's Bearer token to the Hub, which assigns a
//! `server_id` and issues a `server_api_key`.  The library persists those in `data/pairing.json`.
//!
//! Heartbeat: authenticated with `Authorization: Library {server_api_key}` so no user credentials
//! are ever stored on the library server.

use std::path::Path;

use chordia_contracts::catalog::{CatalogPruneRequest, CatalogSyncRequest, CatalogSyncResponse};
use chordia_contracts::directory::{HeartbeatRequest, HeartbeatResponse, ServerOwner};
use chordia_contracts::discord::{
    ArtistArtRequest, ArtistArtResponse, AttributedScrobbleBatch, ListenersNowPlaying,
    PlaylistSearchRequest, PlaylistSearchResponse, PlaylistTracksRequest, PlaylistTracksResponse,
    ResolveListenersRequest, ResolveListenersResponse, ResolveTracksRequest, ResolveTracksResponse,
};
use chordia_contracts::identify::{IdentifyRequest, IdentifyResponse};
use chordia_contracts::scrobble::ScrobbleBatch;
use chordia_contracts::scrobble::ScrobbleBatchResponse;
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

/// What the Hub had to say about a fingerprint.
///
/// Three separate answers because the library does three different things with them, and collapsing
/// any two loses the ability to choose: `Identified` is stored, `NoMatch` is shrugged off until a
/// much later pass, `NotConfigured` stops the identification worker outright. A provider failure is
/// deliberately NOT a variant here — it is an `Err`, so it can never be mistaken for "no data".
#[derive(Debug)]
pub enum IdentifyOutcome {
    Identified(Box<IdentifyResponse>),
    /// AcoustID answered and has never heard this fingerprint.
    NoMatch,
    /// This Hub has no AcoustID key. Nothing is wrong; identification simply is not offered.
    NotConfigured,
}

/// Minimal Hub client - no stored credentials.
pub struct HubClient {
    /// Absent when this library runs with no Hub. See [`base`](HubClient::base) and
    /// `Config::backend_url`.
    base_url: Option<String>,
    http: reqwest::Client,
}

impl HubClient {
    pub fn new(base_url: Option<String>, http: reqwest::Client) -> Self {
        Self { base_url, http }
    }

    /// The Hub's base URL, or an error naming why there is not one.
    ///
    /// Every call below goes through this rather than each caller checking, so a Hub call made in a
    /// Hub-less configuration fails immediately with a sentence that says so — instead of composing
    /// a request against an empty string and failing somewhere in reqwest with a URL parse error.
    /// No caller should reach it: `run_embedded` starts none of the Hub-dependent subsystems, and
    /// the rest already no-op until paired.
    fn base(&self) -> anyhow::Result<&str> {
        self.base_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("this library is configured with no Hub (backend_url)"))
    }

    /// Call `POST /v1/libraries/pair` forwarding the user's access token.
    /// Returns the Hub-assigned server credentials.
    pub async fn pair(&self, user_access_token: &str) -> anyhow::Result<HubPairResponse> {
        let url = format!("{}/v1/libraries/pair", self.base()?);
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

    /// `GET /v1/directory/me`: who owns this server, per the Hub. The Discord bots treat the
    /// owner (through the Discord account linked to their Chordia account) as a bot owner.
    pub async fn server_owner(&self, server_api_key: &str) -> anyhow::Result<ServerOwner> {
        let url = format!("{}/v1/directory/me", self.base()?);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("owner lookup failed {}", resp.status());
        }
        Ok(resp.json::<ServerOwner>().await?)
    }

    /// `POST` a JSON body with the server's own key and decode the JSON answer: the shape of
    /// everything the Discord bot asks the Hub for.
    async fn library_post<Req: serde::Serialize, Res: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        server_api_key: &str,
        body: &Req,
    ) -> anyhow::Result<Res> {
        let url = format!("{}{path}", self.base()?);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .json(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("{path} failed {}", resp.status());
        }
        Ok(resp.json::<Res>().await?)
    }

    /// `POST /v1/directory/now-playing`: what the bot is playing to these listeners, or that it
    /// stopped. No body comes back.
    pub async fn listeners_now_playing(
        &self,
        server_api_key: &str,
        body: &ListenersNowPlaying,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v1/directory/now-playing", self.base()?);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .json(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("now-playing report failed {}", resp.status());
        }
        Ok(())
    }

    /// `POST /v1/directory/listeners:resolve`: which of these Discord users this server may
    /// attribute plays to.
    pub async fn resolve_listeners(
        &self,
        server_api_key: &str,
        req: &ResolveListenersRequest,
    ) -> anyhow::Result<ResolveListenersResponse> {
        self.library_post("/v1/directory/listeners:resolve", server_api_key, req)
            .await
    }

    /// `POST /v1/scrobbles:ingest-attributed`: plays the bot heard listeners hear.
    pub async fn forward_attributed_scrobbles(
        &self,
        server_api_key: &str,
        batch: &AttributedScrobbleBatch,
    ) -> anyhow::Result<ScrobbleBatchResponse> {
        self.library_post("/v1/scrobbles:ingest-attributed", server_api_key, batch)
            .await
    }

    /// `POST /v1/catalog/resolve-tracks`: the Hub's ids for the library's own track ids.
    pub async fn resolve_tracks(
        &self,
        server_api_key: &str,
        req: &ResolveTracksRequest,
    ) -> anyhow::Result<ResolveTracksResponse> {
        self.library_post("/v1/catalog/resolve-tracks", server_api_key, req)
            .await
    }

    /// `POST /v1/catalog/playlists:search`: playlists the bot may queue, by name.
    pub async fn search_playlists(
        &self,
        server_api_key: &str,
        req: &PlaylistSearchRequest,
    ) -> anyhow::Result<PlaylistSearchResponse> {
        self.library_post("/v1/catalog/playlists:search", server_api_key, req)
            .await
    }

    /// `POST /v1/catalog/playlists:tracks`: a playlist's tracks as this server's own refs.
    pub async fn playlist_tracks(
        &self,
        server_api_key: &str,
        req: &PlaylistTracksRequest,
    ) -> anyhow::Result<PlaylistTracksResponse> {
        self.library_post("/v1/catalog/playlists:tracks", server_api_key, req)
            .await
    }

    /// `POST /v1/catalog/artists:art`: an artist's page and pictures.
    pub async fn artists_art(
        &self,
        server_api_key: &str,
        req: &ArtistArtRequest,
    ) -> anyhow::Result<ArtistArtResponse> {
        self.library_post("/v1/catalog/artists:art", server_api_key, req)
            .await
    }

    /// Call `POST /v1/directory/heartbeat` using the server's own API key.
    pub async fn heartbeat(
        &self,
        server_id: Uuid,
        server_api_key: &str,
        endpoint: &str,
        tls_fingerprint: &str,
    ) -> anyhow::Result<u32> {
        let url = format!("{}/v1/directory/heartbeat", self.base()?);
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
        let url = format!("{}/v1/scrobbles:ingest", self.base()?);
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
        let url = format!("{}/v1/catalog/sync", self.base()?);
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

    /// Ask the Hub to identify one acoustic fingerprint (`POST /v1/catalog/identify`), authenticated
    /// with this server's own API key exactly like catalog sync.
    ///
    /// **No audio leaves this host.** What crosses is the Chromaprint string `fpcalc` produced: a
    /// lossy, one-way acoustic hash of a few hundred bytes from which nothing listenable can be
    /// reconstructed. Computing it needs the file and stays here; looking it up is an API call
    /// carrying a hash, and belongs on the Hub — which holds the one AcoustID key, the rate limiter
    /// and the shared cache, so a self-hoster needs no key of their own.
    ///
    /// The three non-error answers stay distinct in the return type, because the caller has to act
    /// differently on each: identified, no match, or "this Hub does not do identification". Anything
    /// else — transport failure, a 5xx, an unparseable body — is an `Err` the caller must treat as
    /// retryable. Folding a failure into [`IdentifyOutcome::NoMatch`] would make a dead provider
    /// indistinguishable from an unidentifiable library.
    pub async fn identify(
        &self,
        server_api_key: &str,
        req: &IdentifyRequest,
    ) -> anyhow::Result<IdentifyOutcome> {
        let url = format!("{}/v1/catalog/identify", self.base()?);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Library {server_api_key}"))
            .json(req)
            .send()
            .await?;
        match resp.status() {
            reqwest::StatusCode::OK => {
                Ok(IdentifyOutcome::Identified(Box::new(resp.json().await?)))
            }
            reqwest::StatusCode::NO_CONTENT => Ok(IdentifyOutcome::NoMatch),
            // A Hub with no AcoustID key is a supported deployment, not a fault: it answers 501 so
            // we can stop asking instead of retrying a question it can never answer.
            reqwest::StatusCode::NOT_IMPLEMENTED => Ok(IdentifyOutcome::NotConfigured),
            status => {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("identify failed {status}: {body}")
            }
        }
    }

    /// Report the authoritative set of track refs so the Hub drops memberships for deleted files.
    pub async fn prune_catalog(
        &self,
        server_api_key: &str,
        req: &CatalogPruneRequest,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v1/catalog/prune", self.base()?);
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
        let url = format!("{}/v1/catalog/covers/{hash}", self.base()?);
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
