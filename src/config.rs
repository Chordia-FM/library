//! TOML configuration for the self-hosted library server.
//!
//! Deliberately minimal - credentials and server identity live in `data/pairing.json`,
//! not in this user-editable file.  The only things that belong here are:
//!   - network settings (port, public endpoint)
//!   - which Hub this server talks to
//!   - where to store data
//!   - log format preference

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub bind_port: u16,
    /// Base URL of the Central Hub.
    pub backend_url: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Frontend URL - where to redirect the browser after the /setup/{token} link is visited.
    #[serde(default = "default_frontend_url")]
    pub frontend_url: String,
    /// Public URL this server is reachable at (advertised to the Hub via heartbeat).
    /// E.g. `http://192.168.1.10:8443` or `https://music.example.com`.
    #[serde(default)]
    pub hub_endpoint: Option<String>,
    #[serde(default = "default_log_format")]
    pub log_format: String,
    /// Where catalog metadata (artists, albums, cover art, …) is stored. `hub` (default) pushes it
    /// to the Central Hub, which enriches it and serves browsing. `local` keeps everything on this
    /// server and the frontend browses it directly.
    #[serde(default)]
    pub metadata_storage: MetadataStorage,
    /// Lower quality-tier transcoding (High/Normal/DataSaver). See [`TranscodeConfig`].
    #[serde(default)]
    pub transcode: TranscodeConfig,
    /// Optional in-process TLS termination. See [`TlsConfig`]. When unset, the server serves plain
    /// HTTP and relies on edge TLS (tunnel / reverse proxy).
    #[serde(default)]
    pub tls: TlsConfig,
    /// Optional AcoustID acoustic-fingerprint identification. See [`AcoustidConfig`].
    #[serde(default)]
    pub acoustid: AcoustidConfig,
    /// EBU R128 / ReplayGain loudness analysis. See [`LoudnessConfig`].
    #[serde(default)]
    pub loudness: LoudnessConfig,
    /// Periodic rescan / prune scheduling. See [`ScanConfig`].
    #[serde(default)]
    pub scan: ScanConfig,
    /// Optional per-response streaming bandwidth cap in kbps. Unset = unlimited (the default). Use
    /// it to bound upload usage on a metered home connection; transcode concurrency is capped
    /// separately under `[transcode] max_concurrent`.
    #[serde(default)]
    pub max_stream_kbps: Option<u32>,
}

/// AcoustID acoustic fingerprinting. When `api_key` is set (and the `fpcalc`/Chromaprint binary is
/// available), a background pass computes each track's fingerprint and resolves it to a stable
/// AcoustID + MusicBrainz recording id - so the same recording matches across different encodings
/// (the preferred own-copy match layer). Disabled when `api_key` is unset.
#[derive(Debug, Clone, Deserialize)]
pub struct AcoustidConfig {
    /// AcoustID application API key (free, from acoustid.org). Identification is off when unset.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Path to the Chromaprint `fpcalc` binary (looked up on `PATH` by default).
    #[serde(default = "default_fpcalc_path")]
    pub fpcalc_path: String,
}

impl Default for AcoustidConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            fpcalc_path: default_fpcalc_path(),
        }
    }
}

/// EBU R128 loudness analysis. A background pass measures each track's integrated loudness + true
/// peak (via `ffmpeg`, reusing `[transcode] ffmpeg_path`) and stores a ReplayGain 2.0 track gain
/// the client applies when "Normalize volume" is on. Enabled by default; set `enabled = false` to
/// skip the analysis entirely (e.g. on a CPU-constrained server).
#[derive(Debug, Clone, Deserialize)]
pub struct LoudnessConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for LoudnessConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Periodic library maintenance. The `notify` watcher handles live filesystem changes; this
/// scheduler additionally re-scans + prunes on an interval to catch changes missed while the server
/// was down (or if a watcher event was dropped). Set `interval_minutes = 0` to disable it.
#[derive(Debug, Clone, Deserialize)]
pub struct ScanConfig {
    #[serde(default = "default_scan_interval")]
    pub interval_minutes: u64,
    /// When the same track is present in multiple encodings (e.g. you re-added an album in higher
    /// quality), keep only the highest-quality copy in the catalog and move the lower-quality
    /// file(s) into a recoverable `superseded/` folder under `data_dir` (never a hard delete).
    /// Matching is high-confidence only (same album + disc + track + title), so remixes/live
    /// versions are never merged. Set false to keep every copy.
    #[serde(default = "default_true")]
    pub dedupe_reuploads: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            interval_minutes: default_scan_interval(),
            dedupe_reuploads: true,
        }
    }
}

/// In-process HTTPS termination. Set both `cert` and `key` (PEM) to serve TLS directly and
/// advertise the leaf-certificate fingerprint to the Hub for pinning. Leave unset to serve plain
/// HTTP behind edge TLS (Cloudflare Tunnel, Caddy, nginx).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TlsConfig {
    /// PEM certificate chain (leaf first).
    #[serde(default)]
    pub cert: Option<PathBuf>,
    /// PEM private key.
    #[serde(default)]
    pub key: Option<PathBuf>,
}

/// On-the-fly transcoding for the non-`Original` quality tiers. Produced by shelling out to
/// `ffmpeg`, cached on disk keyed by `(content_hash, profile)`, and evicted LRU when the cache
/// exceeds [`TranscodeConfig::cache_max_bytes`]. Spatial/Atmos tracks are never transcoded.
#[derive(Debug, Clone, Deserialize)]
pub struct TranscodeConfig {
    /// Path to the `ffmpeg` binary (looked up on `PATH` by default).
    #[serde(default = "default_ffmpeg_path")]
    pub ffmpeg_path: String,
    /// Directory for cached transcoded files. Defaults to `{data_dir}/transcode`.
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    /// Soft cap on the on-disk transcode cache. Oldest (least-recently-served) files are evicted
    /// once the total exceeds this. Default 5 GiB.
    #[serde(default = "default_cache_max_bytes")]
    pub cache_max_bytes: u64,
    /// Maximum number of `ffmpeg` processes running concurrently. Default 2.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            ffmpeg_path: default_ffmpeg_path(),
            cache_dir: None,
            cache_max_bytes: default_cache_max_bytes(),
            max_concurrent: default_max_concurrent(),
        }
    }
}

/// Catalog metadata storage location - see [`Config::metadata_storage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetadataStorage {
    /// Push catalog + artwork to the Hub (default).
    #[default]
    Hub,
    /// Keep catalog metadata on this library server only.
    Local,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var("CHORDIA_LIBRARY_CONFIG")
            .unwrap_or_else(|_| "chordia-library.toml".to_string());
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading config '{path}': {e}"))?;
        let config: Config =
            toml::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing config: {e}"))?;
        Ok(config)
    }

    /// Resolved transcode cache directory: explicit config value, else `{data_dir}/transcode`.
    pub fn transcode_cache_dir(&self) -> PathBuf {
        self.transcode
            .cache_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("transcode"))
    }

    /// `(cert, key)` paths only when both are configured - i.e. in-process TLS is enabled.
    pub fn tls_paths(&self) -> Option<(PathBuf, PathBuf)> {
        match (&self.tls.cert, &self.tls.key) {
            (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
            _ => None,
        }
    }
}

fn default_port() -> u16 {
    8443
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}
fn default_frontend_url() -> String {
    "http://localhost:3000".to_string()
}
fn default_log_format() -> String {
    "pretty".to_string()
}
fn default_ffmpeg_path() -> String {
    "ffmpeg".to_string()
}
fn default_true() -> bool {
    true
}
fn default_scan_interval() -> u64 {
    360 // 6 hours; a full rescan re-hashes files, so keep it infrequent.
}
fn default_fpcalc_path() -> String {
    "fpcalc".to_string()
}
fn default_cache_max_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}
fn default_max_concurrent() -> usize {
    2
}
