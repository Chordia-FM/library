//! Re-upload de-duplication (Phase C6 / user-requested).
//!
//! When the same track exists in several encodings (for example you added an album, then re-added
//! it in higher quality), this keeps only the highest-quality copy in the catalog and moves the
//! lower-quality file(s) into a recoverable `superseded/` folder under the data dir. It is
//! deliberately conservative:
//!   * Match only on high confidence. Copies are grouped by (library, album, disc, track, title)
//!     on the normalized tags, so two genuinely different recordings (a remix, a live take, a
//!     re-recording on a different album) are never merged.
//!   * Never destroy data. The loser is moved to `superseded/`, not deleted. If the move can't be
//!     done safely the copy is left untouched and the catalog keeps both.
//!   * Move before de-indexing. We only drop a copy from the index after its file has left the
//!     scanned tree, so a half-done supersede can't leave a file that just gets re-indexed.
//!
//! Runs as a background pass (has `AppState`), gated by `[scan] dedupe_reuploads` (default on).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::config::Config;
use crate::http::AppState;
use crate::index;

/// Wait between dedupe passes. Longer than a scan so it acts on a settled catalog.
const IDLE_SECS: u64 = 600;
/// Brief delay before the first pass so the initial scan can populate the catalog first.
const STARTUP_DELAY_SECS: u64 = 90;

/// One indexed copy of a track within a library.
#[derive(sqlx::FromRow)]
struct CopyRow {
    library_id: String,
    album_norm: String,
    disc_no: i64,
    track_no: i64,
    title_norm: String,
    /// MusicBrainz recording id, when tagged (Picard). The most reliable cross-encoding identity
    /// (identical for the same recording regardless of codec), so it's the preferred dedupe key.
    recording_mbid: Option<String>,
    /// AcoustID fingerprint id, when computed. A secondary cross-encoding signal, but note the stored
    /// id can differ between encodings of the same recording, so it only pairs copies that happened
    /// to fingerprint to the same id.
    acoustid: Option<String>,
    content_hash: String,
    path: String,
    lossless: i64,
    sample_rate_hz: i64,
    bit_depth: i64,
    channels: i64,
}

/// Comparable quality score: lossless beats lossy; then resolution (rate×depth×channels); then the
/// on-disk file size (a bitrate proxy that separates, say, 320 vs 256 kbps lossy copies). Higher is
/// better.
fn quality_rank(
    lossless: bool,
    sample_rate_hz: i64,
    bit_depth: i64,
    channels: i64,
    size_bytes: i64,
) -> (i64, i64, i64) {
    (
        i64::from(lossless),
        sample_rate_hz * bit_depth.max(1) * channels.max(1),
        size_bytes,
    )
}

/// Spawn the background re-upload dedupe pass. No-op while `[scan] dedupe_reuploads = false`.
pub fn start_dedupe(state: AppState) {
    let cfg: Arc<Config> = state.config.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(STARTUP_DELAY_SECS)).await;
        loop {
            if !cfg.scan.dedupe_reuploads {
                tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                continue;
            }
            match dedupe_pass(&state).await {
                Ok(0) => {}
                Ok(n) => info!(superseded = n, "dedupe: superseded lower-quality copies"),
                Err(e) => warn!(error = %e, "dedupe pass failed"),
            }
            tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
        }
    });
}

/// Run one dedupe pass. Returns how many copies were superseded (moved to trash + de-indexed).
async fn dedupe_pass(state: &AppState) -> anyhow::Result<u32> {
    // When AcoustID is enabled, only act on tracks that have been identified, because we want the
    // authoritative recording id (and track metadata) in hand before deduping, not a guess off
    // half-read tags. Un-identified tracks are skipped this pass and picked up once the AcoustID
    // worker resolves them. Without an AcoustID key, fall back to grouping on whatever we have.
    let require_identified = state.config.acoustid.api_key.is_some();
    let identified_filter = if require_identified {
        " AND t.acoustid IS NOT NULL"
    } else {
        ""
    };
    // album_norm now lives on the albums table (catalog normalization); join for it.
    let rows: Vec<CopyRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT lt.library_id, COALESCE(al.title_normalized, '') AS album_norm, \
                COALESCE(t.disc_no, 0) AS disc_no, COALESCE(t.track_no, 0) AS track_no, \
                t.title_norm, t.recording_mbid, t.acoustid, t.content_hash, \
                fp.path, f.lossless, f.sample_rate_hz, f.bit_depth, f.channels \
         FROM library_tracks lt \
         JOIN tracks t ON t.id = lt.track_id \
         JOIN files f ON f.content_hash = t.content_hash \
         JOIN file_paths fp ON fp.content_hash = t.content_hash AND fp.library_id = lt.library_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         WHERE t.title_norm <> ''{identified_filter}"
    )))
    .fetch_all(&state.db)
    .await?;

    // Group by a per-library identity key. AcoustID (when present) is definitive: it pairs the same
    // recording across encodings even when tags differ (different track numbers, "feat" in titles,
    // etc.). Without it, fall back to the conservative same-album+disc+track+title match (skipped
    // when there's no album, to avoid grouping loose singles). Within a group, collapse to one entry
    // per content_hash (a hash may have multiple paths) and track each copy's best quality and paths.
    struct Entry {
        rank: (i64, i64, i64),
        paths: Vec<String>,
    }
    let mut groups: HashMap<String, HashMap<String, Entry>> = HashMap::new();

    for r in rows {
        // Identity key, most-reliable first: MusicBrainz recording id (stable across encodings),
        // then AcoustID id (pairs copies that fingerprinted alike), then a conservative exact tag
        // match. A copy with none of these can't be grouped safely, so it's left alone.
        let key = if let Some(mbid) = r.recording_mbid.as_deref().filter(|s| !s.is_empty()) {
            format!("{}\u{1}mbid\u{1}{mbid}", r.library_id)
        } else if let Some(aid) = r.acoustid.as_deref().filter(|a| !a.is_empty()) {
            format!("{}\u{1}aid\u{1}{aid}", r.library_id)
        } else if !r.album_norm.is_empty() {
            format!(
                "{}\u{1}tag\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
                r.library_id, r.album_norm, r.disc_no, r.track_no, r.title_norm
            )
        } else {
            continue;
        };
        let size = tokio::fs::metadata(&r.path)
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let rank = quality_rank(
            r.lossless != 0,
            r.sample_rate_hz,
            r.bit_depth,
            r.channels,
            size,
        );
        let by_hash = groups.entry(key).or_default();
        let entry = by_hash.entry(r.content_hash).or_insert(Entry {
            rank,
            paths: Vec::new(),
        });
        entry.rank = entry.rank.max(rank);
        entry.paths.push(r.path);
    }

    let trash_dir = state.config.data_dir.join("superseded");
    let mut superseded = 0u32;

    for (key, by_hash) in groups {
        if by_hash.len() < 2 {
            continue; // a single encoding, nothing to dedupe
        }
        // The library id is the first segment of the composite key.
        let library_id = key.split('\u{1}').next().unwrap_or_default();
        // Keep the highest-ranked copy; supersede the rest.
        let keep = by_hash
            .iter()
            .max_by_key(|(_, e)| e.rank)
            .map(|(h, _)| h.clone())
            .expect("non-empty");
        for (hash, entry) in &by_hash {
            if *hash == keep {
                continue;
            }
            for path in &entry.paths {
                match supersede_one(&state.db, library_id, hash, path, &trash_dir).await {
                    Ok(true) => superseded += 1,
                    Ok(false) => {}
                    Err(e) => warn!(path = %path, error = %e, "dedupe: supersede failed"),
                }
            }
        }
    }
    Ok(superseded)
}

/// Move one lower-quality file to the recoverable trash, then drop it from the index. Returns
/// `Ok(true)` if it was superseded, `Ok(false)` if skipped (for example the move couldn't be done
/// safely, so both copies are left intact). De-index happens only after the file has left the
/// scanned tree.
async fn supersede_one(
    db: &SqlitePool,
    library_id: &str,
    content_hash: &str,
    path: &str,
    trash_dir: &Path,
) -> anyhow::Result<bool> {
    let src = Path::new(path);
    let Some(file_name) = src.file_name() else {
        return Ok(false);
    };
    tokio::fs::create_dir_all(trash_dir).await?;
    // Prefix with a short content-hash slug so two superseded files that share a name never clobber
    // each other in the flat trash folder (each stored copy is uniquely recoverable).
    let slug = &content_hash[..content_hash.len().min(12)];
    let dest: PathBuf = trash_dir.join(format!("{slug}__{}", file_name.to_string_lossy()));

    // Try a rename, then fall back to copy+remove (handles a cross-device data_dir). If neither
    // works, leave the file (and the index) untouched rather than risk losing it or causing
    // re-index churn.
    if tokio::fs::rename(src, &dest).await.is_err() {
        match tokio::fs::copy(src, &dest).await {
            Ok(_) => {
                if let Err(e) = tokio::fs::remove_file(src).await {
                    // Copied but couldn't remove the original, so undo the copy to avoid duplicating.
                    let _ = tokio::fs::remove_file(&dest).await;
                    return Err(anyhow::anyhow!("removing superseded original: {e}"));
                }
            }
            Err(e) => return Err(anyhow::anyhow!("moving to trash: {e}")),
        }
    }

    // The file has left the scanned tree; now drop it from the index for this library.
    index::remove_track(db, library_id, src).await?;
    info!(from = %path, to = ?dest, "dedupe: superseded lower-quality copy");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_beats_lossy() {
        let flac = quality_rank(true, 44_100, 16, 2, 30_000_000);
        let mp3 = quality_rank(false, 44_100, 16, 2, 9_000_000);
        assert!(flac > mp3);
    }

    #[test]
    fn higher_resolution_wins_among_lossless() {
        let hi = quality_rank(true, 96_000, 24, 2, 80_000_000);
        let cd = quality_rank(true, 44_100, 16, 2, 30_000_000);
        assert!(hi > cd);
    }

    #[test]
    fn file_size_breaks_lossy_ties() {
        // Same codec params (say, two MP3s) means the larger file (higher bitrate) wins.
        let b320 = quality_rank(false, 44_100, 16, 2, 9_600_000);
        let b256 = quality_rank(false, 44_100, 16, 2, 7_700_000);
        assert!(b320 > b256);
    }
}
