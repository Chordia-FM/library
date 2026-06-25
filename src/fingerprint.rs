//! AcoustID acoustic fingerprinting (Phase A5).
//!
//! Computes a Chromaprint fingerprint per track (shelling out to `fpcalc`, like the transcoder
//! shells to `ffmpeg`) and resolves it via the AcoustID web service to a stable AcoustID id and
//! MusicBrainz recording id. Because two different encodings of the same recording resolve to
//! the same AcoustID id, storing it lets [`crate::catalog::find_track_by_acoustid`] match an
//! owned copy across encodings. This is the preferred own-copy layer (above content-hash and fuzzy).
//!
//! Runs as a background pass (no scan-loop coupling), rate-limited to respect the AcoustID API.
//! Entirely optional: disabled unless `[acoustid] api_key` is set, and each track is skipped
//! cleanly if `fpcalc` is missing or the lookup finds nothing.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tracing::{info, warn};

use crate::config::Config;
use crate::http::AppState;
use crate::metadata::normalize;

/// How many unidentified tracks to attempt per pass.
const BATCH: i64 = 25;
/// Idle wait when there's nothing to do (or identification is disabled).
const IDLE_SECS: u64 = 300;
/// Spacing between AcoustID requests (the service asks for ≤3 req/s; we stay well under).
const REQ_SPACING: Duration = Duration::from_millis(400);

/// A computed Chromaprint fingerprint.
pub struct Fingerprint {
    pub duration_secs: u32,
    pub fingerprint: String,
}

/// A resolved acoustic identity, enriched with the authoritative release context (the metadata
/// organize/dedupe need: a stable recording id + track position + album).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub acoustid: String,
    pub recording_mbid: Option<String>,
    /// Album the matched release belongs to (canonical title from MusicBrainz).
    pub album: Option<String>,
    pub release_mbid: Option<String>,
    pub year: Option<i64>,
    /// Track and medium position on the matched release (the track/disc numbers untagged files lack).
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
}

/// Run `fpcalc -json <path>` and parse its `{duration, fingerprint}` output.
pub async fn compute(path: &Path, fpcalc_path: &str) -> anyhow::Result<Fingerprint> {
    #[derive(Deserialize)]
    struct FpcalcOut {
        duration: f64,
        fingerprint: String,
    }
    let out = tokio::process::Command::new(fpcalc_path)
        .arg("-json")
        .arg(path)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawning fpcalc ({fpcalc_path}): {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "fpcalc failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let parsed: FpcalcOut = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("parsing fpcalc output: {e}"))?;
    Ok(Fingerprint {
        duration_secs: parsed.duration.round().max(0.0) as u32,
        fingerprint: parsed.fingerprint,
    })
}

/// True when a release's title normalizes to the wanted album (and the wanted album isn't blank).
fn release_matches_album(rel: &serde_json::Value, album_norm: &str) -> bool {
    !album_norm.is_empty()
        && rel
            .get("title")
            .and_then(|t| t.as_str())
            .map(|t| normalize(t) == album_norm)
            .unwrap_or(false)
}

/// A recording has a release matching the wanted album.
fn recording_matches_album(rec: &serde_json::Value, album_norm: &str) -> bool {
    rec.get("releases")
        .and_then(|r| r.as_array())
        .map(|rels| {
            rels.iter()
                .any(|rel| release_matches_album(rel, album_norm))
        })
        .unwrap_or(false)
}

/// A recording's title normalizes to the wanted title.
fn recording_matches_title(rec: &serde_json::Value, title_norm: &str) -> bool {
    !title_norm.is_empty()
        && rec
            .get("title")
            .and_then(|t| t.as_str())
            .map(|t| normalize(t) == title_norm)
            .unwrap_or(false)
}

/// Extract `(disc_no, track_no)` from the first medium of a release that lists this recording's
/// track (AcoustID nests just our track under each medium).
fn release_position(rel: &serde_json::Value) -> (Option<i64>, Option<i64>) {
    let Some(mediums) = rel.get("mediums").and_then(|m| m.as_array()) else {
        return (None, None);
    };
    for m in mediums {
        if let Some(track) = m
            .get("tracks")
            .and_then(|t| t.as_array())
            .and_then(|ts| ts.first())
        {
            let disc = m.get("position").and_then(|p| p.as_i64());
            let track_no = track.get("position").and_then(|p| p.as_i64());
            if track_no.is_some() {
                return (disc, track_no);
            }
        }
    }
    (None, None)
}

/// Parse an AcoustID `v2/lookup` (rich meta) response into an enriched [`Identity`]. From the
/// highest-scoring result it picks the recording that best matches the file we're identifying,
/// preferring one whose release matches the file's album tag (this is what makes two encodings of
/// the same track on the same album converge on one `recording_mbid`), then a title match, then the
/// first. It pulls the track/disc position and album from the matching release. Pure for testing.
fn parse_lookup(
    body: &serde_json::Value,
    want_title_norm: &str,
    want_album_norm: &str,
) -> Option<Identity> {
    if body.get("status").and_then(|s| s.as_str()) != Some("ok") {
        return None;
    }
    let results = body.get("results")?.as_array()?;
    let best = results.iter().max_by(|a, b| {
        let sa = a.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let sb = b.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let acoustid = best.get("id")?.as_str()?.to_string();

    let empty = Vec::new();
    let recordings = best
        .get("recordings")
        .and_then(|r| r.as_array())
        .unwrap_or(&empty);

    // Album match is the strongest signal that two encodings refer to the same track; combine with
    // a title match when possible, then degrade gracefully.
    let rec = recordings
        .iter()
        .find(|r| {
            recording_matches_album(r, want_album_norm)
                && recording_matches_title(r, want_title_norm)
        })
        .or_else(|| {
            recordings
                .iter()
                .find(|r| recording_matches_album(r, want_album_norm))
        })
        .or_else(|| {
            recordings
                .iter()
                .find(|r| recording_matches_title(r, want_title_norm))
        })
        .or_else(|| recordings.first());

    let mut id = Identity {
        acoustid,
        ..Default::default()
    };
    if let Some(rec) = rec {
        id.recording_mbid = rec.get("id").and_then(|i| i.as_str()).map(String::from);
        // Prefer the release matching the album tag; else the first listed.
        let rel = rec
            .get("releases")
            .and_then(|r| r.as_array())
            .and_then(|rels| {
                rels.iter()
                    .find(|rel| release_matches_album(rel, want_album_norm))
                    .or_else(|| rels.first())
            });
        if let Some(rel) = rel {
            id.album = rel.get("title").and_then(|t| t.as_str()).map(String::from);
            id.release_mbid = rel.get("id").and_then(|i| i.as_str()).map(String::from);
            id.year = rel
                .get("date")
                .and_then(|d| d.get("year"))
                .and_then(|y| y.as_i64());
            let (disc, track) = release_position(rel);
            id.disc_no = disc;
            id.track_no = track;
        }
    }
    Some(id)
}

/// Resolve a fingerprint to an enriched AcoustID identity via the web service. `title`/`album` are
/// the file's current tags, used to pick the right recording + release among the candidates.
pub async fn lookup(
    http: &reqwest::Client,
    api_key: &str,
    fp: &Fingerprint,
    title: &str,
    album: &str,
) -> anyhow::Result<Option<Identity>> {
    let duration = fp.duration_secs.to_string();
    let url = reqwest::Url::parse_with_params(
        "https://api.acoustid.org/v2/lookup",
        &[
            ("client", api_key),
            ("duration", duration.as_str()),
            ("fingerprint", fp.fingerprint.as_str()),
            // Rich meta so we get the recording's releases + this track's position on them.
            ("meta", "recordings releases tracks"),
        ],
    )
    .map_err(|e| anyhow::anyhow!("building acoustid url: {e}"))?;
    let body: serde_json::Value = http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_lookup(&body, &normalize(title), &normalize(album)))
}

/// Spawn the background AcoustID identification pass. No-op while `api_key` is unset.
pub fn start_identification(state: AppState) {
    let cfg: Arc<Config> = state.config.clone();
    tokio::spawn(async move {
        loop {
            let Some(api_key) = cfg.acoustid.api_key.clone() else {
                // Disabled: sleep long; nothing to do without a key.
                tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                continue;
            };

            match identify_batch(&state, &api_key, &cfg.acoustid.fpcalc_path).await {
                Ok(0) => tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await,
                Ok(n) => info!(identified = n, "acoustid: identified tracks"),
                Err(e) => {
                    warn!(error = %e, "acoustid pass failed");
                    tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                }
            }
        }
    });
}

/// One track awaiting identification: its current tags (to disambiguate the AcoustID candidates) and
/// a library and path (to fingerprint and, once enriched, organize on disk).
#[derive(sqlx::FromRow)]
struct PendingRow {
    id: String,
    title: String,
    album: String,
    album_id: Option<String>,
    library_id: String,
    path: String,
}

/// Identify up to `BATCH` tracks that have no AcoustID yet, storing the authoritative recording id,
/// track/disc position, and album info (which the metadata organize/dedupe need), then placing the
/// file on disk now that it's complete. Returns how many were resolved.
async fn identify_batch(state: &AppState, api_key: &str, fpcalc_path: &str) -> anyhow::Result<u32> {
    let rows: Vec<PendingRow> = sqlx::query_as(
        "SELECT t.id, t.title, COALESCE(al.title, '') AS album, t.album_id, \
                lt.library_id, fp.path \
         FROM tracks t \
         JOIN file_paths fp ON fp.content_hash = t.content_hash \
         JOIN library_tracks lt ON lt.track_id = t.id AND lt.library_id = fp.library_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         WHERE t.acoustid IS NULL \
         GROUP BY t.id LIMIT ?",
    )
    .bind(BATCH)
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let mut resolved = 0u32;
    for r in rows {
        let fp = match compute(Path::new(&r.path), fpcalc_path).await {
            Ok(fp) => fp,
            Err(e) => {
                warn!(track = %r.id, error = %e, "fpcalc failed - skipping");
                continue;
            }
        };
        tokio::time::sleep(REQ_SPACING).await;
        match lookup(&state.http, api_key, &fp, &r.title, &r.album).await {
            Ok(Some(identity)) => {
                // recording_mbid is overwritten (the album/title-matched pick is more reliable than
                // any prior guess); track/disc backfill only where the file tags lack them.
                sqlx::query(
                    "UPDATE tracks SET acoustid = ?, \
                     recording_mbid = COALESCE(?, recording_mbid), \
                     track_no = COALESCE(track_no, ?), disc_no = COALESCE(disc_no, ?) WHERE id = ?",
                )
                .bind(&identity.acoustid)
                .bind(identity.recording_mbid.as_deref())
                .bind(identity.track_no)
                .bind(identity.disc_no)
                .bind(&r.id)
                .execute(&state.db)
                .await?;
                // Backfill album-level facts (year / release id) onto the album.
                if let Some(album_id) = &r.album_id {
                    sqlx::query(
                        "UPDATE albums SET year = COALESCE(year, ?), \
                         release_mbid = COALESCE(release_mbid, ?) WHERE id = ?",
                    )
                    .bind(identity.year)
                    .bind(identity.release_mbid.as_deref())
                    .bind(album_id)
                    .execute(&state.db)
                    .await?;
                }
                resolved += 1;

                // Now that the track has its real metadata, place it on disk (organize was gated
                // earlier when the track number was missing).
                if let Some((root, settings)) =
                    crate::organize::library_settings(&state.db, &r.library_id).await
                {
                    if let Err(e) = crate::organize::organize_file(
                        &state.db,
                        &r.library_id,
                        &root,
                        &settings,
                        Path::new(&r.path),
                    )
                    .await
                    {
                        warn!(track = %r.id, error = %e, "organize after identify failed");
                    }
                }
            }
            Ok(None) => { /* no match, leave NULL and retry on a future run */ }
            Err(e) => warn!(track = %r.id, error = %e, "acoustid lookup failed"),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_highest_scoring_result() {
        let body = serde_json::json!({
            "status": "ok",
            "results": [
                { "id": "low-score", "score": 0.3, "recordings": [{ "id": "mbid-a" }] },
                { "id": "best-id",   "score": 0.91, "recordings": [{ "id": "mbid-b" }] }
            ]
        });
        let id = parse_lookup(&body, "", "").expect("should identify");
        assert_eq!(id.acoustid, "best-id");
        assert_eq!(id.recording_mbid.as_deref(), Some("mbid-b"));
    }

    #[test]
    fn handles_no_recordings_and_errors() {
        // A result without recordings → acoustid set, mbid None.
        let no_rec = serde_json::json!({
            "status": "ok",
            "results": [{ "id": "only-acoustid", "score": 0.8 }]
        });
        let id = parse_lookup(&no_rec, "", "").unwrap();
        assert_eq!(id.acoustid, "only-acoustid");
        assert_eq!(id.recording_mbid, None);

        // Non-ok status or empty results → None.
        assert!(parse_lookup(&serde_json::json!({ "status": "error" }), "", "").is_none());
        assert!(parse_lookup(
            &serde_json::json!({ "status": "ok", "results": [] }),
            "",
            ""
        )
        .is_none());
    }

    #[test]
    fn picks_album_matched_recording_and_position() {
        // Mirrors the real AcoustID shape for "Diablo": several candidate recordings; the right one
        // is the one whose release matches the file's album tag. That's how a FLAC and MP3 of the
        // same track converge on one recording id.
        let body = serde_json::json!({
            "status": "ok",
            "results": [{
                "id": "acid-1", "score": 0.99,
                "recordings": [
                    { "id": "wrong-comp", "title": "Diablo",
                      "releases": [{ "title": "Faces Era", "id": "rel-era", "date": {"year": 2021} }] },
                    { "id": "right-rec", "title": "Diablo",
                      "releases": [{ "title": "Faces", "id": "rel-faces", "date": {"year": 2014},
                                     "mediums": [{ "position": 1, "tracks": [{ "position": 13 }] }] }] }
                ]
            }]
        });
        let id = parse_lookup(&body, &normalize("Diablo"), &normalize("Faces")).unwrap();
        assert_eq!(id.recording_mbid.as_deref(), Some("right-rec"));
        assert_eq!(id.album.as_deref(), Some("Faces"));
        assert_eq!(id.release_mbid.as_deref(), Some("rel-faces"));
        assert_eq!(id.year, Some(2014));
        assert_eq!(id.track_no, Some(13));
        assert_eq!(id.disc_no, Some(1));
    }
}
