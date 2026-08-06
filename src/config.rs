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
    /// Optional torrent-based acquisition (Lidarr-style). See [`AcquisitionConfig`]. Off unless
    /// `enabled = true` and a Prowlarr + qBittorrent are configured.
    #[serde(default)]
    pub acquisition: AcquisitionConfig,
}

/// Torrent-based acquisition. The library executes download jobs pulled from the Hub (and the
/// interactive direct-search endpoint): it searches a user-supplied **Prowlarr** for releases,
/// scores them against the job's quality profile, hands the chosen one to a user-supplied
/// **qBittorrent**, then imports the finished files into the target library's music folder so the
/// scanner picks them up. Chordia ships NO indexers or trackers; the operator configures their own.
/// Disabled unless `enabled = true` and the Prowlarr/qBittorrent URLs are set.
#[derive(Debug, Clone, Deserialize)]
pub struct AcquisitionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub prowlarr_url: Option<String>,
    #[serde(default)]
    pub prowlarr_api_key: Option<String>,
    #[serde(default)]
    pub qbit_url: Option<String>,
    #[serde(default)]
    pub qbit_user: Option<String>,
    #[serde(default)]
    pub qbit_pass: Option<String>,
    /// Where qBittorrent saves downloads. MUST share a filesystem/volume with the music root so the
    /// import is an atomic move. Defaults to `{data_dir}/acquisition`.
    #[serde(default)]
    pub staging_dir: Option<PathBuf>,
    /// Subfolder under the target library's music root that finished downloads are moved into (the
    /// scanner then indexes + organizes them). Default `_incoming`.
    #[serde(default = "default_import_subdir")]
    pub import_subdir: String,
    /// How often the job loop polls the Hub for queued jobs (seconds).
    #[serde(default = "default_acq_poll_secs")]
    pub poll_interval_secs: u64,
    /// Keep seeding finished torrents (the imported files are hardlinked/copied so the torrent keeps
    /// its own copy). When false, the torrent's files are MOVED into the library and the torrent is
    /// removed from qBittorrent once imported. Default true.
    #[serde(default = "default_keep_seeding")]
    pub keep_seeding: bool,
    /// Remote / shared-seedbox mode: qBittorrent runs elsewhere (e.g. a seedbox) and this library
    /// reads finished files from a LOCAL MOUNT of its download directory. Set BOTH `remote_path` (the
    /// path qBittorrent reports, e.g. `/downloads`) and `local_path` (where that's mounted here, e.g.
    /// `/mnt/seedbox` or `D:\seedbox`). In this mode files are COPIED (never moved) and the torrent is
    /// left seeding on the seedbox. `remote_path` is also the save path handed to qBittorrent.
    #[serde(default)]
    pub remote_path: Option<String>,
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    /// qBittorrent category for THIS library's grabs, so a shared seedbox can tell libraries apart and
    /// each can monitor/import only its own torrents. Default `chordia`.
    #[serde(default)]
    pub category: Option<String>,
    /// How often (days) the Hub re-searches a followed-artist release that wasn't found on the
    /// trackers yet (reuses the existing job, never a duplicate). Reported to the Hub on the health
    /// heartbeat. Default 7.
    #[serde(default = "default_research_interval_days")]
    pub research_interval_days: u32,
    /// Quality-upgrade sweep: periodically propose re-acquiring the worst all-lossy albums in
    /// better (lossless) quality. On by default whenever acquisition itself is configured.
    #[serde(default = "default_upgrade_enabled")]
    pub upgrade_enabled: bool,
    /// Albums proposed per sweep (worst-first). Keeps each pass a small burst of tracker searches
    /// instead of a collection-wide hammering. Default 10.
    #[serde(default = "default_upgrade_cap")]
    pub upgrade_cap: u32,
    /// Days between sweeps. Default 7.
    #[serde(default = "default_upgrade_interval_days")]
    pub upgrade_interval_days: u32,
    /// Days before a previously-proposed album may be proposed again. This is what ROTATES the
    /// sweep through the collection (never the same worst-10 every week) and what eventually
    /// re-checks old attempts once new sources may exist. Default 90.
    #[serde(default = "default_upgrade_retry_days")]
    pub upgrade_retry_days: u32,
}

impl Default for AcquisitionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prowlarr_url: None,
            prowlarr_api_key: None,
            qbit_url: None,
            qbit_user: None,
            qbit_pass: None,
            staging_dir: None,
            import_subdir: default_import_subdir(),
            poll_interval_secs: default_acq_poll_secs(),
            keep_seeding: default_keep_seeding(),
            remote_path: None,
            local_path: None,
            category: None,
            research_interval_days: default_research_interval_days(),
            upgrade_enabled: default_upgrade_enabled(),
            upgrade_cap: default_upgrade_cap(),
            upgrade_interval_days: default_upgrade_interval_days(),
            upgrade_retry_days: default_upgrade_retry_days(),
        }
    }
}

fn default_keep_seeding() -> bool {
    true
}

fn default_research_interval_days() -> u32 {
    7
}

fn default_upgrade_enabled() -> bool {
    true
}

fn default_upgrade_cap() -> u32 {
    10
}

fn default_upgrade_interval_days() -> u32 {
    7
}

fn default_upgrade_retry_days() -> u32 {
    90
}

impl AcquisitionConfig {
    /// True when acquisition is enabled AND the minimum external infra is configured.
    pub fn is_configured(&self) -> bool {
        self.enabled
            && self.prowlarr_url.is_some()
            && self.prowlarr_api_key.is_some()
            && self.qbit_url.is_some()
    }

    /// Remote / shared-seedbox mode (both `remote_path` and `local_path` set): qBittorrent runs
    /// elsewhere and finished files are read from a local mount. Drives copy-not-move + path mapping.
    pub fn is_remote(&self) -> bool {
        self.remote_path.is_some() && self.local_path.is_some()
    }

    /// The qBittorrent category to file this library's grabs under (default `chordia`).
    pub fn category(&self) -> &str {
        self.category
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("chordia")
    }

    /// Map a qBittorrent-reported content path to one readable by THIS library. In remote mode the
    /// `remote_path` prefix is rewritten to `local_path` (separators normalised, so a Linux seedbox
    /// path like `/downloads/Album` maps onto a Windows mount); otherwise the path is unchanged.
    pub fn local_content_path(&self, content_path: &str) -> String {
        match (&self.remote_path, &self.local_path) {
            (Some(remote), Some(local)) => {
                let rel = content_path
                    .strip_prefix(remote.as_str())
                    .unwrap_or(content_path)
                    .trim_start_matches(['/', '\\']);
                let mut p = local.clone();
                for c in rel.split(['/', '\\']).filter(|s| !s.is_empty()) {
                    p.push(c);
                }
                p.to_string_lossy().into_owned()
            }
            _ => content_path.to_string(),
        }
    }
}

/// Acoustic fingerprinting. A background pass computes each track's Chromaprint fingerprint and
/// asks the paired Hub to resolve it to a stable AcoustID + MusicBrainz recording id - so the same
/// recording matches across different encodings (the preferred own-copy match layer).
///
/// **A paired library needs no key here, and that is the point.** Identification used to be gated
/// behind this library's own `api_key`; essentially no self-hoster set one, so untagged imports
/// stayed untagged forever on every instance. A Hub already owns third-party provider access, so it
/// holds the key and identifies on behalf of every library paired to it — nothing to configure.
///
/// `api_key` remains as the **standalone** path. A library is meant to be a complete music server on
/// its own: client plus library, no Hub. Making identification reachable only through a Hub would
/// have made a core capability — untagged imports getting an album, track numbers and artwork —
/// something you cannot have without joining someone's network, which is the opposite of the point.
/// So set it only if you run unpaired, or on local metadata storage, or your Hub has no key.
///
/// Hub first when one is available (it caches, shares a rate budget and needs no setup); this key is
/// the fallback. `fpcalc_path` is the one knob both paths need, because that binary reads the audio
/// and so must run locally.
#[derive(Debug, Clone, Deserialize)]
pub struct AcoustidConfig {
    /// Path to the Chromaprint `fpcalc` binary (looked up on `PATH` by default).
    #[serde(default = "default_fpcalc_path")]
    pub fpcalc_path: String,
    /// AcoustID application key (free, from <https://acoustid.org/new-application>) for identifying
    /// without a Hub. Unset is correct for a paired library.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl Default for AcoustidConfig {
    fn default() -> Self {
        Self {
            fpcalc_path: default_fpcalc_path(),
            api_key: None,
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

    /// Resolved acquisition staging directory: explicit config value, else `{data_dir}/acquisition`.
    pub fn acquisition_staging_dir(&self) -> PathBuf {
        self.acquisition
            .staging_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("acquisition"))
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
fn default_import_subdir() -> String {
    "_incoming".to_string()
}
fn default_acq_poll_secs() -> u64 {
    20
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acq(remote: Option<&str>, local: Option<&str>) -> AcquisitionConfig {
        AcquisitionConfig {
            remote_path: remote.map(String::from),
            local_path: local.map(PathBuf::from),
            ..Default::default()
        }
    }

    #[test]
    fn local_mode_passes_content_path_through() {
        let c = acq(None, None);
        assert!(!c.is_remote());
        assert_eq!(c.local_content_path("/anything/here"), "/anything/here");
    }

    #[test]
    fn remote_mode_remaps_the_seedbox_prefix_to_the_local_mount() {
        let c = acq(Some("/downloads"), Some("/mnt/seedbox"));
        assert!(c.is_remote());
        let mapped = c.local_content_path("/downloads/Some Album (2020)/cd1");
        let expected = PathBuf::from("/mnt/seedbox")
            .join("Some Album (2020)")
            .join("cd1")
            .to_string_lossy()
            .into_owned();
        assert_eq!(mapped, expected);
    }

    #[test]
    fn remote_mode_needs_both_paths() {
        // Only one of the pair set → not remote mode (treated as local, path unchanged).
        assert!(!acq(Some("/downloads"), None).is_remote());
        assert!(!acq(None, Some("/mnt/seedbox")).is_remote());
        assert_eq!(
            acq(Some("/downloads"), None).local_content_path("/downloads/x"),
            "/downloads/x"
        );
    }

    #[test]
    fn category_defaults_to_chordia_but_is_overridable() {
        assert_eq!(acq(None, None).category(), "chordia");
        let mut c = acq(None, None);
        c.category = Some("lib-frankfurt".into());
        assert_eq!(c.category(), "lib-frankfurt");
        c.category = Some(String::new()); // empty → default
        assert_eq!(c.category(), "chordia");
    }
}
