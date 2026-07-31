//! Torrent-based acquisition executor (Lidarr-style).
//!
//! The library pulls download jobs from the Hub's queue, searches a user-supplied **Prowlarr** for
//! releases, scores them against the job's quality profile, hands the winner to a user-supplied
//! **qBittorrent**, then imports the finished files into the target library's music folder (the
//! scanner indexes them and `catalog_sync` pushes them back to the Hub). Chordia ships no indexers
//! or trackers; the operator configures their own. Everything here is off unless `[acquisition]
//! enabled = true` with Prowlarr + qBittorrent configured.

pub mod pipeline;
pub mod quality;
pub mod upgrade;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chordia_contracts::acquisition::{AcquisitionReport, JobStatusUpdate};
use reqwest::StatusCode;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::AcquisitionConfig;
use crate::http::AppState;
use crate::pairing::HubClient;

/// A release returned by Prowlarr search.
#[derive(Debug, Clone)]
pub struct Release {
    pub guid: String,
    pub title: String,
    pub download_url: Option<String>,
    pub magnet_url: Option<String>,
    pub info_hash: Option<String>,
    pub size: i64,
    pub seeders: i32,
    pub leechers: i32,
    pub indexer: Option<String>,
}

/// qBittorrent torrent state used to track a grab to completion.
#[derive(Debug, Clone)]
pub struct TorrentInfo {
    pub progress: f32,
    pub state: String,
    pub content_path: String,
    /// Seeds currently connected (0 for a long stretch ⇒ a stalled download worth abandoning).
    pub num_seeds: i64,
}

impl TorrentInfo {
    pub fn is_complete(&self) -> bool {
        self.progress >= 1.0
            || matches!(
                self.state.as_str(),
                "uploading" | "stalledUP" | "pausedUP" | "forcedUP" | "queuedUP" | "checkingUP"
            )
    }
}

/// A self-contained torrent source to hand qBittorrent. Already fetched/resolved by the library, so
/// qBittorrent never has to reach the indexer.
enum TorrentSource {
    /// A magnet link (added via `urls`).
    Magnet(String),
    /// Raw `.torrent` file bytes (added via a multipart file upload).
    File(Vec<u8>),
}

/// A client over the operator's Prowlarr + qBittorrent. Holds a qBittorrent session cookie.
pub struct AcquisitionClient {
    http: reqwest::Client,
    /// A second client that does NOT auto-follow redirects, so a Prowlarr download link that 302s to a
    /// `magnet:` URI (which `reqwest` can't follow) is caught rather than erroring.
    dl_http: reqwest::Client,
    prowlarr_url: String,
    prowlarr_key: String,
    qbit_url: String,
    qbit_user: Option<String>,
    qbit_pass: Option<String>,
    /// Remote/shared-seedbox mode: finished files live on a mount this library only COPIES from and
    /// never owns, so a teardown must never delete them (see `remove_on_teardown`).
    remote: bool,
    /// The WebUI session cookie as a full `name=value` pair. The cookie NAME varies by qBittorrent
    /// version (`SID` on old builds, `QBT_SID_<port>` on 5.x+), so we store and replay it verbatim.
    session: Mutex<Option<String>>,
}

impl AcquisitionClient {
    /// Build a client when the config has the minimum infra; `None` otherwise.
    pub fn from_config(cfg: &AcquisitionConfig) -> Option<Self> {
        if !cfg.is_configured() {
            return None;
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .ok()?;
        let dl_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        Some(Self {
            http,
            dl_http,
            prowlarr_url: cfg.prowlarr_url.clone()?,
            prowlarr_key: cfg.prowlarr_api_key.clone()?,
            qbit_url: cfg.qbit_url.clone()?,
            qbit_user: cfg.qbit_user.clone(),
            qbit_pass: cfg.qbit_pass.clone(),
            remote: cfg.is_remote(),
            session: Mutex::new(None),
        })
    }

    // ── Prowlarr ─────────────────────────────────────────────────────────────

    /// GET a Prowlarr endpoint with the API key. Surfaces the REAL failure cause — reqwest's own
    /// message is just the opaque `error sending request for url (…)`; the timeout / connect / TLS
    /// reason lives in its error `source()` chain — and retries a transient connection blip twice
    /// (Prowlarr fans each search out across every indexer, so a momentary hiccup is common). A
    /// genuine timeout fails fast: re-running a 60s search that's actually slow just wastes minutes.
    async fn prowlarr_get(&self, url: &str) -> anyhow::Result<reqwest::Response> {
        let mut last = String::new();
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
            }
            match self
                .http
                .get(url)
                .header("X-Api-Key", &self.prowlarr_key)
                .send()
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(e) if e.is_timeout() => {
                    anyhow::bail!("prowlarr request timed out (60s): {}", err_chain(&e))
                }
                Err(e) => last = err_chain(&e),
            }
        }
        anyhow::bail!("prowlarr request failed after 3 tries: {last}")
    }

    /// Search Prowlarr's audio categories for `query`.
    pub async fn search(&self, query: &str) -> anyhow::Result<Vec<Release>> {
        let url = format!(
            "{}/api/v1/search?type=search&categories=3000&query={}",
            self.prowlarr_url.trim_end_matches('/'),
            urlencode(query)
        );
        let resp = self.prowlarr_get(&url).await?;
        if !resp.status().is_success() {
            anyhow::bail!("prowlarr search failed {}", resp.status());
        }
        let arr: Vec<Value> = resp.json().await?;
        Ok(arr.iter().filter_map(release_from_json).collect())
    }

    /// Number of indexers Prowlarr has configured (for the health report).
    pub async fn indexer_count(&self) -> anyhow::Result<u32> {
        let url = format!("{}/api/v1/indexer", self.prowlarr_url.trim_end_matches('/'));
        let resp = self.prowlarr_get(&url).await?;
        if !resp.status().is_success() {
            anyhow::bail!("prowlarr indexer list failed {}", resp.status());
        }
        let arr: Vec<Value> = resp.json().await?;
        Ok(arr.len() as u32)
    }

    // ── qBittorrent ──────────────────────────────────────────────────────────

    async fn ensure_login(&self) -> anyhow::Result<()> {
        if self.session.lock().await.is_some() {
            return Ok(());
        }
        let url = format!("{}/api/v2/auth/login", self.qbit_url.trim_end_matches('/'));
        let body = form_encode(&[
            ("username", self.qbit_user.as_deref().unwrap_or("")),
            ("password", self.qbit_pass.as_deref().unwrap_or("")),
        ]);
        let resp = self
            .http
            .post(&url)
            .header("Referer", &self.qbit_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await?;
        // Capture the session cookie as a full `name=value` pair. The name varies by qBittorrent
        // version (`SID` on old builds, `QBT_SID_<port>` on 5.x+), so match ANY `*SID*` cookie rather
        // than assuming `SID`. Some setups (whitelisted / auth-bypassed) instead return "Ok." with no
        // cookie at all.
        for hv in resp.headers().get_all(reqwest::header::SET_COOKIE) {
            if let Some(pair) = hv
                .to_str()
                .ok()
                .and_then(|s| s.split(';').next())
                .map(|c| c.trim().to_string())
                .filter(|c| {
                    c.split_once('=')
                        .is_some_and(|(name, _)| name.contains("SID"))
                })
            {
                *self.session.lock().await = Some(pair);
                return Ok(());
            }
        }
        let body = resp.text().await.unwrap_or_default();
        if body.contains("Ok") {
            return Ok(());
        }
        anyhow::bail!("qBittorrent login failed");
    }

    async fn cookie(&self) -> Option<String> {
        self.session.lock().await.clone()
    }

    /// POST a urlencoded form to qBittorrent with the session cookie, re-logging in once on 403.
    async fn qbit_form(
        &self,
        path: &str,
        form: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response> {
        self.ensure_login().await?;
        let url = format!("{}{path}", self.qbit_url.trim_end_matches('/'));
        let body = form_encode(form);
        let send = |cookie: Option<String>, body: String| {
            let mut req = self
                .http
                .post(&url)
                .header("Referer", &self.qbit_url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(body);
            if let Some(c) = cookie {
                req = req.header(reqwest::header::COOKIE, c);
            }
            req.send()
        };
        let resp = send(self.cookie().await, body.clone()).await?;
        if resp.status() == StatusCode::FORBIDDEN {
            *self.session.lock().await = None;
            self.ensure_login().await?;
            return Ok(send(self.cookie().await, body).await?);
        }
        Ok(resp)
    }

    async fn qbit_get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response> {
        self.ensure_login().await?;
        let url = format!(
            "{}{path}?{}",
            self.qbit_url.trim_end_matches('/'),
            form_encode(query)
        );
        let mut req = self.http.get(&url).header("Referer", &self.qbit_url);
        if let Some(c) = self.cookie().await {
            req = req.header(reqwest::header::COOKIE, c);
        }
        Ok(req.send().await?)
    }

    /// Add a release to qBittorrent under `category`, **tagged** with `tag` (a per-job token so that only
    /// we can identify it again on a possibly-SHARED qBittorrent), saving into `savepath`. Returns
    /// its infohash.
    pub async fn grab(
        &self,
        release: &Release,
        savepath: &str,
        category: &str,
        tag: &str,
    ) -> anyhow::Result<String> {
        // Resolve a SELF-CONTAINED source ourselves (a magnet, or the `.torrent` bytes) by fetching the
        // indexer link from the LIBRARY side (which can always reach Prowlarr). qBittorrent is then handed
        // a magnet or the torrent file directly and never has to contact the indexer itself, so this works
        // even when qBittorrent is on a remote seedbox / behind a VPN / behind indexer auth.
        let source = self.resolve_source(release).await?;

        // A magnet's btih is an authoritative HINT; otherwise we rely on the unique `tag` (and the
        // before/after hash diff as a last resort).
        let known = match &source {
            TorrentSource::Magnet(m) => parse_btih(m),
            TorrentSource::File(_) => None,
        };

        // Snapshot existing hashes BEFORE adding, as a fallback to the tag lookup.
        let before = self.all_hashes().await.unwrap_or_default();

        let resp = match &source {
            TorrentSource::Magnet(magnet) => {
                self.qbit_form(
                    "/api/v2/torrents/add",
                    &[
                        ("urls", magnet.as_str()),
                        ("savepath", savepath),
                        ("category", category),
                        ("tags", tag),
                        ("autoTMM", "false"),
                    ],
                )
                .await?
            }
            TorrentSource::File(bytes) => {
                self.qbit_add_file(bytes, savepath, category, tag).await?
            }
        };
        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent add failed {}", resp.status());
        }

        // Resolve the infohash qBittorrent ACTUALLY registered. PREFER the unique tag: unambiguous even
        // when several libraries add to the same qBittorrent at once. Then a magnet's btih once present
        // (authoritative, and handles a shared qBittorrent deduping our add onto an existing torrent).
        // Last, the newly-appeared hash, for a `.torrent` file whose real hash we can't know up front
        // (and the advertised infoHash can be wrong, which would have us track a phantom).
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(Some(h)) = self.hash_by_tag(tag).await {
                return Ok(h);
            }
            let now = self.all_hashes().await.unwrap_or_default();
            if let Some(h) = known.as_ref().filter(|h| now.contains(*h)) {
                return Ok(h.clone());
            }
            if let Some(h) = now.difference(&before).next() {
                return Ok(h.clone());
            }
        }
        anyhow::bail!(
            "torrent did not appear in qBittorrent after the add. The indexer's download link may be \
             unreachable from qBittorrent or require authentication"
        )
    }

    /// The infohash (lowercased) of the torrent carrying `tag`, if any. This is how we pick out our
    /// just-added torrent on a shared qBittorrent without racing other libraries' concurrent adds.
    async fn hash_by_tag(&self, tag: &str) -> anyhow::Result<Option<String>> {
        let resp = self
            .qbit_get("/api/v2/torrents/info", &[("tag", tag)])
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent info-by-tag failed {}", resp.status());
        }
        let arr: Vec<Value> = resp.json().await?;
        Ok(arr
            .first()
            .and_then(|t| t["hash"].as_str())
            .map(|s| s.to_lowercase()))
    }

    /// Resolve a release to a self-contained [`TorrentSource`] WITHOUT involving qBittorrent: fetch the
    /// indexer's download link from OUR side (Prowlarr is always reachable from the library) and return
    /// either the magnet it points to or the `.torrent` bytes. Falls back to the release's own magnet.
    async fn resolve_source(&self, release: &Release) -> anyhow::Result<TorrentSource> {
        if let Some(url) = release.download_url.as_deref().filter(|s| !s.is_empty()) {
            match self.fetch_source(url).await {
                Ok(src) => return Ok(src),
                Err(e) => {
                    tracing::warn!(error = %e, "indexer download link fetch failed; trying the magnet");
                }
            }
        }
        if let Some(m) = release.magnet_url.as_deref().filter(|s| !s.is_empty()) {
            return Ok(TorrentSource::Magnet(m.to_string()));
        }
        anyhow::bail!("release has no reachable download link or magnet")
    }

    /// Fetch an indexer download URL and classify the result: a `magnet:` redirect/body → a magnet; any
    /// other body → `.torrent` file bytes. Follows ONE http redirect (e.g. Prowlarr → the indexer file).
    async fn fetch_source(&self, url: &str) -> anyhow::Result<TorrentSource> {
        let resp = self
            .dl_http
            .get(url)
            .header("X-Api-Key", &self.prowlarr_key)
            .send()
            .await?;
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if loc.is_empty() {
                anyhow::bail!("indexer download redirected with no location");
            }
            if loc.starts_with("magnet:") {
                return Ok(TorrentSource::Magnet(loc));
            }
            // Only re-attach the Prowlarr API key if the redirect stayed on Prowlarr. The location
            // comes from the indexer response, so an indexer (or anyone who can answer as one) could
            // point it at a host of their choosing and be handed the key — which is full control of
            // the user's Prowlarr, including its other indexer credentials.
            let same_origin = reqwest::Url::parse(&loc).ok().is_some_and(|target| {
                reqwest::Url::parse(&self.prowlarr_url)
                    .ok()
                    .is_some_and(|base| target.origin() == base.origin())
            });
            let mut req = self.http.get(&loc);
            if same_origin {
                req = req.header("X-Api-Key", &self.prowlarr_key);
            }
            let resp = req.send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("indexer download failed {}", resp.status());
            }
            return Ok(classify_source(resp.bytes().await?.to_vec()));
        }
        if !resp.status().is_success() {
            anyhow::bail!("indexer download failed {}", resp.status());
        }
        Ok(classify_source(resp.bytes().await?.to_vec()))
    }

    /// Add a `.torrent` FILE to qBittorrent via a multipart upload (qBittorrent never fetches it itself),
    /// re-logging in once on a 403.
    async fn qbit_add_file(
        &self,
        torrent: &[u8],
        savepath: &str,
        category: &str,
        tag: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.ensure_login().await?;
        let url = format!(
            "{}/api/v2/torrents/add",
            self.qbit_url.trim_end_matches('/')
        );
        const BOUNDARY: &str = "----chordiaAcqBoundaryq1w2e3r4t5y6u7";
        let body = build_multipart(BOUNDARY, torrent, savepath, category, tag);
        let send = |cookie: Option<String>, body: Vec<u8>| {
            let mut req = self
                .http
                .post(&url)
                .header("Referer", &self.qbit_url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(body);
            if let Some(c) = cookie {
                req = req.header(reqwest::header::COOKIE, c);
            }
            req.send()
        };
        let resp = send(self.cookie().await, body.clone()).await?;
        if resp.status() == StatusCode::FORBIDDEN {
            *self.session.lock().await = None;
            self.ensure_login().await?;
            return Ok(send(self.cookie().await, body).await?);
        }
        Ok(resp)
    }

    /// Current state of a torrent by infohash.
    pub async fn info(&self, hash: &str) -> anyhow::Result<Option<TorrentInfo>> {
        let resp = self
            .qbit_get("/api/v2/torrents/info", &[("hashes", hash)])
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent info failed {}", resp.status());
        }
        let arr: Vec<Value> = resp.json().await?;
        Ok(arr.first().map(|t| TorrentInfo {
            progress: t["progress"].as_f64().unwrap_or(0.0) as f32,
            state: t["state"].as_str().unwrap_or_default().to_string(),
            content_path: t["content_path"]
                .as_str()
                .or_else(|| t["save_path"].as_str())
                .unwrap_or_default()
                .to_string(),
            num_seeds: t["num_seeds"].as_i64().unwrap_or(0),
        }))
    }

    /// The torrent's contained file names (relative paths), available as soon as its METADATA has
    /// resolved — long before any content downloads. Empty until then (magnets need a peer to hand
    /// over metadata). Powers the early wrong-release check: a mislabelled torrent is caught from
    /// its file list in seconds instead of after a full download.
    pub async fn torrent_files(&self, hash: &str) -> anyhow::Result<Vec<String>> {
        let resp = self
            .qbit_get("/api/v2/torrents/files", &[("hash", hash)])
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent files failed {}", resp.status());
        }
        let arr: Vec<Value> = resp.json().await?;
        Ok(arr
            .iter()
            .filter_map(|f| f["name"].as_str().map(String::from))
            .collect())
    }

    /// Remove a torrent from qBittorrent. `delete_files` also deletes its on-disk data, so pass false
    /// when we've already moved the files into the library.
    pub async fn remove_torrent(&self, hash: &str, delete_files: bool) -> anyhow::Result<()> {
        let resp = self
            .qbit_form(
                "/api/v2/torrents/delete",
                &[
                    ("hashes", hash),
                    ("deleteFiles", if delete_files { "true" } else { "false" }),
                ],
            )
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("qBittorrent delete failed {}", resp.status());
        }
        Ok(())
    }

    /// Remove a torrent during TEARDOWN (failure, cancel, abandon, or pre-grab cleanup), deleting its
    /// files too — EXCEPT in remote/shared-seedbox mode, where the files live on a mount this library
    /// only copies from and never owns, so a teardown must never wipe the seedbox's data.
    pub async fn remove_on_teardown(&self, hash: &str) -> anyhow::Result<()> {
        self.remove_torrent(hash, !self.remote).await
    }

    /// Every torrent infohash currently in qBittorrent (lowercased), for detecting a just-added one.
    async fn all_hashes(&self) -> anyhow::Result<HashSet<String>> {
        let resp = self.qbit_get("/api/v2/torrents/info", &[]).await?;
        let arr: Vec<Value> = resp.json().await?;
        Ok(arr
            .iter()
            .filter_map(|t| t["hash"].as_str().map(|s| s.to_lowercase()))
            .collect())
    }
}

/// Render an error plus its full `source()` chain. A reqwest transport failure's top-level `Display`
/// is only `error sending request for url (…)`; the actual cause (`operation timed out`, `tcp connect
/// error: …`, a TLS error) is a nested source, so we walk and join the chain for a usable message.
fn err_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        src = inner.source();
    }
    s
}

fn release_from_json(r: &Value) -> Option<Release> {
    let guid = r["guid"].as_str()?.to_string();
    let title = r["title"].as_str()?.to_string();
    if guid.is_empty() || title.is_empty() {
        return None;
    }
    let magnet = r["magnetUrl"].as_str().map(String::from).or_else(|| {
        r["guid"]
            .as_str()
            .filter(|g| g.starts_with("magnet:"))
            .map(String::from)
    });
    Some(Release {
        guid,
        title,
        download_url: r["downloadUrl"].as_str().map(String::from),
        magnet_url: magnet,
        info_hash: r["infoHash"].as_str().map(String::from),
        size: r["size"].as_i64().unwrap_or(0),
        seeders: r["seeders"].as_i64().unwrap_or(0) as i32,
        leechers: r["leechers"].as_i64().unwrap_or(0) as i32,
        indexer: r["indexer"].as_str().map(String::from),
    })
}

/// Minimal `application/x-www-form-urlencoded` percent-encoding (the library's `reqwest` is built
/// without the helpers that would do this, so we encode query/form values ourselves).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Classify fetched download-link bytes: a `magnet:` body → a magnet source; anything else → `.torrent`
/// file bytes.
fn classify_source(bytes: Vec<u8>) -> TorrentSource {
    if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"magnet:") {
        TorrentSource::Magnet(String::from_utf8_lossy(&bytes).trim().to_string())
    } else {
        TorrentSource::File(bytes)
    }
}

/// Build a `multipart/form-data` body for qBittorrent's `/torrents/add`, carrying the `.torrent` file
/// plus the save-path / category / tags fields. Binary-safe: the torrent bytes are spliced in verbatim.
fn build_multipart(
    boundary: &str,
    torrent: &[u8],
    savepath: &str,
    category: &str,
    tag: &str,
) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::with_capacity(torrent.len() + 512);
    b.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; \
             filename=\"download.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(torrent);
    b.extend_from_slice(b"\r\n");
    for (name, value) in [
        ("savepath", savepath),
        ("category", category),
        ("tags", tag),
        ("autoTMM", "false"),
    ] {
        b.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    b
}

/// Extract the v1 infohash (`xt=urn:btih:<hex>`) from a magnet URI, lowercased.
fn parse_btih(magnet: &str) -> Option<String> {
    let lower = magnet.to_lowercase();
    let idx = lower.find("urn:btih:")? + "urn:btih:".len();
    let rest = &lower[idx..];
    let hash: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    // Only accept a 40-char hex v1 hash (qBittorrent keys on this).
    (hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit())).then_some(hash)
}

// ── Status-update constructors ───────────────────────────────────────────────

/// A bare status transition.
pub fn status(s: &str) -> JobStatusUpdate {
    JobStatusUpdate {
        status: s.to_string(),
        progress: None,
        chosen_guid: None,
        chosen_title: None,
        quality_label: None,
        score: None,
        size_bytes: None,
        seeders: None,
        qbit_hash: None,
        error: None,
        detail: None,
    }
}

/// A failure transition with a message.
pub fn failed(msg: &str) -> JobStatusUpdate {
    let mut u = status("failed");
    u.error = Some(msg.to_string());
    u
}

// ── Resume bookkeeping ───────────────────────────────────────────────────────

/// Record an in-flight grab so a restart can re-attach its monitor.
pub async fn record_job(
    db: &SqlitePool,
    job_id: Uuid,
    qbit_hash: &str,
    hub_library_id: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO acquisition_jobs (job_id, qbit_hash, hub_library_id) VALUES (?, ?, ?)",
    )
    .bind(job_id.to_string())
    .bind(qbit_hash)
    .bind(hub_library_id.to_string())
    .execute(db)
    .await?;
    Ok(())
}

/// The qBittorrent hash recorded for this job's in-flight grab, if any. Used to tear down the torrent
/// when a job is cancelled/failed, and to clear a stale grab before a retry re-grabs.
pub async fn prior_hash(db: &SqlitePool, job_id: Uuid) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT qbit_hash FROM acquisition_jobs WHERE job_id = ?")
            .bind(job_id.to_string())
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(h,)| h))
}

/// Forget a finished/failed grab.
pub async fn clear_job(db: &SqlitePool, job_id: Uuid) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM acquisition_jobs WHERE job_id = ?")
        .bind(job_id.to_string())
        .execute(db)
        .await?;
    Ok(())
}

/// In-flight grabs to resume on startup: `(job_id, qbit_hash, hub_library_id)`.
pub async fn pending_jobs(db: &SqlitePool) -> anyhow::Result<Vec<(Uuid, String, Uuid)>> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT job_id, qbit_hash, hub_library_id FROM acquisition_jobs")
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(j, h, l)| Some((j.parse().ok()?, h, l.parse().ok()?)))
        .collect())
}

/// `(local_library_id, music_root)` for a Hub library id hosted on this server, if any.
pub async fn local_library(
    db: &SqlitePool,
    hub_library_id: Uuid,
) -> anyhow::Result<Option<(String, PathBuf)>> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id, path FROM libraries WHERE hub_library_id = ?")
            .bind(hub_library_id.to_string())
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(id, path)| (id, PathBuf::from(path))))
}

// ── Per-job concurrency guard ────────────────────────────────────────────────

/// RAII claim that a single run/resume flow owns a download `job_id`. While held, `claim_job` returns
/// `None` for the same id, so a re-queue + re-claim (or a startup resume racing a fresh claim) can't
/// spawn a second monitor that would tear down the first flow's torrent (the bookkeeping is keyed on
/// job_id alone, so two concurrent flows would clobber each other). Released when the owning task ends,
/// including on panic.
pub struct JobGuard {
    // Fully-qualified: the unqualified `Mutex` in this module is tokio's async one (used by the
    // qBittorrent session); the in-flight set is a plain sync mutex.
    set: Arc<std::sync::Mutex<HashSet<Uuid>>>,
    job_id: Uuid,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.job_id);
        }
    }
}

/// Claim a job for exclusive processing, or `None` if another flow already holds it.
pub fn claim_job(state: &AppState, job_id: Uuid) -> Option<JobGuard> {
    let mut set = state.inflight_jobs.lock().ok()?;
    if !set.insert(job_id) {
        return None;
    }
    Some(JobGuard {
        set: state.inflight_jobs.clone(),
        job_id,
    })
}

// ── Background loops ─────────────────────────────────────────────────────────

/// Poll the Hub for queued jobs and run each to completion. No-op unless configured + paired.
pub fn start_job_loop(state: AppState) {
    if !state.config.acquisition.is_configured() {
        tracing::info!("acquisition not configured; job loop disabled");
        return;
    }
    tokio::spawn(async move {
        tracing::info!("acquisition job loop starting");
        tokio::time::sleep(Duration::from_secs(15)).await;
        let interval = state.config.acquisition.poll_interval_secs.max(5);
        loop {
            if let Err(e) = poll_once(&state).await {
                tracing::warn!(error = %e, "acquisition poll failed");
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}

async fn poll_once(state: &AppState) -> anyhow::Result<()> {
    let Some(creds) = state.credentials.read().await.clone() else {
        return Ok(());
    };
    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());
    let jobs = hub
        .claim_jobs(&creds.server_api_key, creds.server_id, 3)
        .await?;
    for job in jobs {
        // Skip a job already owned by a live run/resume flow (e.g. the reaper re-queued a download we
        // are still monitoring) so we never run a second monitor against the same job.
        let Some(guard) = claim_job(state, job.job_id) else {
            continue;
        };
        let st = state.clone();
        tokio::spawn(async move {
            let _guard = guard; // freed (job_id released) when this task ends, incl. on panic
            pipeline::run_job(&st, job).await;
        });
    }
    Ok(())
}

/// On startup, re-attach the monitor to any downloads that were in flight when the library last
/// stopped, so a restart doesn't orphan a grab.
pub fn start_resume(state: AppState) {
    if !state.config.acquisition.is_configured() {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        match pending_jobs(&state.db).await {
            Ok(jobs) => {
                for (job_id, hash, hub_lib) in jobs {
                    // A fresh claim of this same job (a retry) may already own it. Skip to avoid a
                    // second monitor on the stale hash.
                    let Some(guard) = claim_job(&state, job_id) else {
                        continue;
                    };
                    tracing::info!(job = %job_id, "resuming in-flight download");
                    let st = state.clone();
                    tokio::spawn(async move {
                        let _guard = guard;
                        pipeline::resume_job(&st, job_id, hash, hub_lib).await;
                    });
                }
            }
            Err(e) => tracing::warn!(error = %e, "acquisition resume scan failed"),
        }
    });
}

/// Periodically report acquisition health to the Hub so the UI can gate downloads.
pub fn start_report_loop(state: AppState) {
    if !state.config.acquisition.enabled {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            if let Err(e) = report_once(&state).await {
                tracing::debug!(error = %e, "acquisition health report failed");
            }
            tokio::time::sleep(Duration::from_secs(120)).await;
        }
    });
}

async fn report_once(state: &AppState) -> anyhow::Result<()> {
    let Some(creds) = state.credentials.read().await.clone() else {
        return Ok(());
    };
    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());
    let (enabled, indexer_count, error) =
        match AcquisitionClient::from_config(&state.config.acquisition) {
            Some(c) => match c.indexer_count().await {
                Ok(n) => (true, n, None),
                Err(e) => (false, 0, Some(e.to_string())),
            },
            None => (false, 0, None),
        };
    let libs: Vec<(String,)> =
        sqlx::query_as("SELECT hub_library_id FROM libraries WHERE hub_library_id IS NOT NULL")
            .fetch_all(&state.db)
            .await?;
    let report = AcquisitionReport {
        enabled,
        indexer_count,
        client_kind: Some("qbittorrent".to_string()),
        error,
        research_interval_days: state.config.acquisition.research_interval_days,
    };
    for (hub_id,) in libs {
        if let Ok(uuid) = hub_id.parse::<Uuid>() {
            let _ = hub
                .report_acquisition(&creds.server_api_key, uuid, &report)
                .await;
        }
    }
    Ok(())
}
