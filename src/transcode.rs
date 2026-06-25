//! On-the-fly transcoding for the non-`Original` quality tiers (M6 / Phase A1).
//!
//! Lower tiers are produced by shelling out to `ffmpeg` (no library dependency - most servers
//! already ship it), written fully to a cache file under `{data_dir}/transcode`, then served via
//! the same [`crate::streaming::serve_range`] path as original files (so seeking/Range works).
//! Cache files are keyed by `(content_hash, profile)`; the cache is evicted least-recently-served
//! first once it exceeds [`crate::config::TranscodeConfig::cache_max_bytes`].
//!
//! Spatial/Atmos tracks are **never** transcoded - the streaming handler forces `Original` for
//! them - so the lossless/passthrough promise is never silently broken.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use chordia_contracts::streaming::QualityProfile;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config::TranscodeConfig;
use crate::error::{AppError, AppResult};

/// Concrete encoding parameters a non-`Original` tier maps to.
struct Target {
    /// Stable slug used in the cache filename + ETag (matches the serde representation).
    slug: &'static str,
    /// Output container extension.
    ext: &'static str,
    /// ffmpeg muxer (`-f`). Passed explicitly so the muxer never depends on the (temp) filename.
    format: &'static str,
    /// ffmpeg `-c:a` value.
    encoder: &'static str,
    /// ffmpeg `-b:a` value.
    bitrate: &'static str,
    /// Codec string handed to `serve_range` so it picks the right `Content-Type`.
    content_codec: &'static str,
}

fn target_for(profile: QualityProfile) -> Option<Target> {
    match profile {
        // Original is passthrough - the caller serves the source file directly.
        QualityProfile::Original => None,
        QualityProfile::High => Some(Target {
            slug: "high",
            ext: "m4a",
            format: "mp4",
            encoder: "aac",
            bitrate: "256k",
            content_codec: "aac",
        }),
        QualityProfile::Normal => Some(Target {
            slug: "normal",
            ext: "m4a",
            format: "mp4",
            encoder: "aac",
            bitrate: "128k",
            content_codec: "aac",
        }),
        QualityProfile::DataSaver => Some(Target {
            slug: "data_saver",
            ext: "ogg",
            format: "ogg",
            encoder: "libopus",
            bitrate: "96k",
            content_codec: "opus",
        }),
    }
}

/// Result of resolving a transcoded stream: the cache file, the codec string for `Content-Type`,
/// and a profile-qualified ETag (so a client doesn't reuse a cached `Original` body for `high`).
pub struct Transcoded {
    pub path: PathBuf,
    pub content_codec: &'static str,
    pub etag: String,
}

pub struct Transcoder {
    cache_dir: PathBuf,
    ffmpeg: String,
    max_bytes: u64,
    sem: Semaphore,
    /// In-process last-served times for LRU ordering; falls back to file mtime across restarts.
    last_access: Mutex<HashMap<PathBuf, Instant>>,
    /// Monotonic counter giving every transcode attempt a unique temp filename, so two concurrent
    /// transcodes of the same (hash, profile) never write the same temp file (which would corrupt
    /// the published cache entry).
    tmp_seq: std::sync::atomic::AtomicU64,
}

impl Transcoder {
    pub fn new(cfg: &TranscodeConfig, cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            ffmpeg: cfg.ffmpeg_path.clone(),
            max_bytes: cfg.cache_max_bytes,
            sem: Semaphore::new(cfg.max_concurrent.max(1)),
            last_access: Mutex::new(HashMap::new()),
            tmp_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn touch(&self, path: &Path) {
        if let Ok(mut map) = self.last_access.lock() {
            map.insert(path.to_path_buf(), Instant::now());
        }
    }

    /// Ensure a transcoded copy of `source` at `profile` exists on disk and return how to serve it.
    /// Returns `Ok(None)` for `Original` (caller serves the source bytes unaltered).
    pub async fn ensure(
        &self,
        source: &Path,
        content_hash: &str,
        profile: QualityProfile,
    ) -> AppResult<Option<Transcoded>> {
        let Some(t) = target_for(profile) else {
            return Ok(None);
        };
        let file_name = format!("{content_hash}.{}.{}", t.slug, t.ext);
        let out = self.cache_dir.join(&file_name);
        let etag = format!("{content_hash}.{}", t.slug);

        if tokio::fs::try_exists(&out).await.unwrap_or(false) {
            self.touch(&out);
            return Ok(Some(Transcoded {
                path: out,
                content_codec: t.content_codec,
                etag,
            }));
        }

        // One ffmpeg per (cache miss) slot. Re-check after acquiring: another request for the same
        // (hash, profile) may have finished it while we waited.
        let _permit = self
            .sem
            .acquire()
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
        if tokio::fs::try_exists(&out).await.unwrap_or(false) {
            self.touch(&out);
            return Ok(Some(Transcoded {
                path: out,
                content_codec: t.content_codec,
                etag,
            }));
        }

        // Transcode to a temp file, then atomically rename so a crash/cancel never leaves a
        // truncated file that would later be served as if complete.
        let nonce = self
            .tmp_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self
            .cache_dir
            .join(format!("{file_name}.{}.{nonce}.tmp", std::process::id()));

        let mut cmd = tokio::process::Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
            .arg(source)
            // Drop any video/cover stream; map only audio; re-encode to the tier.
            .args([
                "-vn",
                "-map_metadata",
                "0",
                "-c:a",
                t.encoder,
                "-b:a",
                t.bitrate,
            ]);
        if t.format == "mp4" {
            // Put the moov atom at the front so byte-range seeking works on the cached file.
            cmd.args(["-movflags", "+faststart"]);
        }
        // Set the muxer explicitly - the temp filename has no usable extension.
        cmd.args(["-f", t.format]).arg(&tmp);

        let status = cmd.status().await.map_err(|e| {
            AppError::Internal(anyhow::anyhow!("spawning ffmpeg ({}): {e}", self.ffmpeg))
        })?;

        if !status.success() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(AppError::BadGateway(format!(
                "transcode to {} failed (ffmpeg exit {:?})",
                t.slug,
                status.code()
            )));
        }

        tokio::fs::rename(&tmp, &out)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("publishing transcode: {e}")))?;
        self.touch(&out);
        info!(content_hash, profile = t.slug, "transcoded");

        self.enforce_budget().await;
        Ok(Some(Transcoded {
            path: out,
            content_codec: t.content_codec,
            etag,
        }))
    }

    /// Evict least-recently-served cache files until the total is under `max_bytes`.
    async fn enforce_budget(&self) {
        let mut entries: Vec<(PathBuf, u64, Option<Instant>)> = Vec::new();
        let mut total: u64 = 0;
        let Ok(mut rd) = tokio::fs::read_dir(&self.cache_dir).await else {
            return;
        };
        // Snapshot in-process access times so we can rank entries we've served this run.
        let access = self
            .last_access
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        while let Ok(Some(ent)) = rd.next_entry().await {
            let path = ent.path();
            if path.extension().is_some_and(|e| e == "tmp") {
                continue;
            }
            let Ok(meta) = ent.metadata().await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let len = meta.len();
            total += len;
            // Rank by last-served time; files not served this run (`None`) sort oldest.
            let rank = access.get(&path).copied();
            entries.push((path, len, rank));
        }

        if total <= self.max_bytes {
            return;
        }
        // Oldest first: `None` (unseen) precedes any `Some(instant)`, then earliest instant.
        entries.sort_by_key(|e| e.2);
        for (path, len, _) in entries {
            if total <= self.max_bytes {
                break;
            }
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    total = total.saturating_sub(len);
                    if let Ok(mut map) = self.last_access.lock() {
                        map.remove(&path);
                    }
                }
                Err(e) => warn!(?path, error = %e, "transcode cache eviction failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chordia-tc-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn original_tier_is_passthrough() {
        let dir = temp_dir("orig");
        let tc = Transcoder::new(&TranscodeConfig::default(), dir.clone());
        // Original must never transcode - the caller serves the source bytes directly.
        let res = tc
            .ensure(
                Path::new("does-not-exist"),
                "hash",
                QualityProfile::Original,
            )
            .await
            .unwrap();
        assert!(res.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn transcodes_then_serves_from_cache() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not on PATH - skipping transcode integration test");
            return;
        }
        let dir = temp_dir("xcode");
        let src = dir.join("src.flac");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-ac",
                "2",
                "-c:a",
                "flac",
            ])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(made, "failed to synthesize test source");

        let tc = Transcoder::new(&TranscodeConfig::default(), dir.clone());

        let first = tc
            .ensure(&src, "abc123", QualityProfile::DataSaver)
            .await
            .unwrap()
            .expect("data_saver tier must transcode");
        assert!(first.path.exists());
        assert!(std::fs::metadata(&first.path).unwrap().len() > 0);
        assert_eq!(first.content_codec, "opus");
        assert!(first.etag.ends_with("data_saver"));

        // A second request for the same (hash, profile) reuses the cache file.
        let second = tc
            .ensure(&src, "abc123", QualityProfile::DataSaver)
            .await
            .unwrap()
            .expect("cached");
        assert_eq!(first.path, second.path);

        // A different profile produces a distinct cache file + ETag.
        let high = tc
            .ensure(&src, "abc123", QualityProfile::High)
            .await
            .unwrap()
            .expect("high tier must transcode");
        assert_ne!(high.path, first.path);
        assert_eq!(high.content_codec, "aac");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
