//! Acoustic fingerprinting and track identification.
//!
//! Two halves, split along the one boundary that matters:
//!
//! * [`compute`] shells out to `fpcalc` (Chromaprint) to turn a decoded track into a fingerprint.
//!   **This is the only half that touches audio, and it stays here, on the machine that owns the
//!   file.** Nothing downstream ever sees a sample.
//! * The lookup — fingerprint → AcoustID → MusicBrainz recording. What leaves this host is the
//!   Chromaprint string: a lossy, one-way hash of a few hundred bytes describing the track's coarse
//!   spectral shape over time. Nothing listenable can be reconstructed from it, which is why this
//!   call may cross a boundary audio never may.
//!
//! # Two ways to do the lookup, and why both exist
//!
//! The lookup used to be here only, gated behind this library's own `[acoustid] api_key`.
//! Essentially no self-hoster sets one, so the feature was off by default for everyone: a Manager
//! download of FLACs carrying only ARTIST and TITLE imported 33 tracks with no album and no artwork,
//! and not one was ever fingerprinted, because the key that would have allowed it did not exist on
//! that instance or on effectively any other. A Hub already holds third-party provider credentials,
//! a shared rate limiter and a response cache, so it holds an AcoustID key too and identifies for
//! every library paired to it. **A paired library needs no configuration at all** — that is the
//! point of [`Resolver::Hub`].
//!
//! But a library is meant to be a complete music server by itself: client plus library, no Hub. If
//! the Hub were the *only* route, then getting an album, track numbers and artwork onto an untagged
//! import would require joining someone's network — a core capability held hostage to a component
//! the design says you should be able to do without. So [`Resolver::Local`] stays: set
//! `[acoustid] api_key` and this library identifies entirely on its own.
//!
//! The Hub is preferred when both are available, and a Hub that turns out to have no key falls back
//! to the local key rather than giving up. See [`Resolver::pick`].
//!
//! Because two encodings of the same recording produce the same AcoustID id, storing it lets
//! [`crate::catalog::find_track_by_acoustid`] match an owned copy across encodings — the preferred
//! own-copy layer, above content-hash and fuzzy matching.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chordia_contracts::identify::{IdentifyRequest, IdentifyResponse};
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::MetadataStorage;
use crate::http::AppState;
use crate::metadata::normalize;
use crate::pairing::{HubClient, IdentifyOutcome, PairingCredentials};

/// How many unidentified tracks to attempt per pass.
const BATCH: i64 = 25;
/// Idle wait when there's nothing to do (or identification isn't available).
const IDLE_SECS: u64 = 300;
/// Minimum gap between identify requests, in milliseconds.
///
/// **On [`Resolver::Local`] this is AcoustID's published rate limit and not a courtesy**: at most
/// 3 requests/second per application key, and exceeding it earns a 429 and eventually a revoked key.
/// One spawned loop issues at most one request per iteration, so this spacing IS the rate limit —
/// there is no other gate behind it on that route.
///
/// On [`Resolver::Hub`] it paces calls to the Hub instead, which spends one key across every library
/// attached to it and queues on its own gate regardless; the spacing here keeps one library's
/// backlog from arriving as a burst that every other library then waits behind.
///
/// 400 ms = 2.5 req/s, deliberately under the ceiling rather than exactly at it.
const REQ_SPACING_MS: u64 = 400;

/// AcoustID's documented ceiling. Encoded so the assertion below can be read against the source.
const ACOUSTID_MAX_RPS: u64 = 3;

// A guard, not decoration: [`REQ_SPACING_MS`] is the ONLY thing standing between the standalone path
// and a rate-limit violation, and "make identification faster" is an obvious-looking edit that would
// breach it silently — the failure arrives later, as someone else's revoked key.
const _: () = assert!(
    REQ_SPACING_MS > 1000 / ACOUSTID_MAX_RPS,
    "REQ_SPACING_MS would exceed AcoustID's 3 requests/second limit"
);

const REQ_SPACING: Duration = Duration::from_millis(REQ_SPACING_MS);

/// A computed Chromaprint fingerprint. Produced locally; only ever leaves this host as the two
/// scalars below, never as audio.
pub struct Fingerprint {
    pub duration_secs: u32,
    pub fingerprint: String,
}

/// Run `fpcalc -json <path>` and parse its `{duration, fingerprint}` output.
///
/// Unchanged by the move to Hub-side lookup, and deliberately so: this is the step that opens the
/// file, so this is the step that must stay on the library host.
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

/// Who answers "what recording is this fingerprint?".
enum Resolver {
    /// Ask the paired Hub. Preferred: no key to configure here, and the Hub caches responses and
    /// spends one rate budget across every library attached to it.
    Hub(Box<PairingCredentials>),
    /// Call AcoustID directly with this library's own key. The standalone path — a library with no
    /// Hub is still a complete music server, so it still identifies.
    Local(String),
}

impl Resolver {
    /// Choose how to identify right now, or `None` when neither route is open.
    ///
    /// Re-evaluated every pass rather than fixed at startup, because both inputs change while we
    /// run: pairing can complete, and `hub_has_key` flips the moment a Hub answers 501.
    async fn pick(state: &AppState, hub_has_key: bool) -> Option<Self> {
        let local = state
            .config
            .acoustid
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty());

        // A Hub only answers for libraries whose metadata it actually stores.
        if hub_has_key && state.config.metadata_storage == MetadataStorage::Hub {
            if let Some(creds) = state.credentials.read().await.clone() {
                return Some(Self::Hub(Box::new(creds)));
            }
        }
        local.map(Self::Local)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Hub(_) => "hub",
            Self::Local(_) => "local key",
        }
    }
}

/// Spawn the background identification pass.
///
/// A paired library needs no configuration. An unpaired one — or one on local metadata storage, or
/// one whose Hub has no key — identifies through `[acoustid] api_key` instead, and only goes quiet
/// when neither route is open.
pub fn start_identification(state: AppState) {
    tokio::spawn(async move {
        // Flipped false the first time a Hub answers 501. That is a property of the Hub, not of any
        // one track, so it is worth remembering: it stops us re-asking a guaranteed-501 question
        // every five minutes, and it is what promotes the local key to the active route.
        let mut hub_has_key = true;
        loop {
            let Some(resolver) = Resolver::pick(&state, hub_has_key).await else {
                tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                continue;
            };

            match identify_batch(&state, &resolver).await {
                Ok(0) => tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await,
                Ok(n) => {
                    info!(
                        identified = n,
                        via = resolver.label(),
                        "acoustid: identified tracks"
                    )
                }
                // Not an error: this Hub does not offer identification. Stop asking it — but only
                // the Hub is ruled out, not identification itself. If a local key is configured the
                // next pass picks it up and carries on; otherwise `pick` returns None and we idle,
                // which still beats returning, because a key can be added and reloaded later.
                Err(e) if e.is::<NotConfigured>() => {
                    hub_has_key = false;
                    state.identify_available.store(false, Ordering::Relaxed);
                    if state.config.acoustid.api_key.is_some() {
                        info!(
                            "the Hub has no AcoustID key; falling back to this library's own key"
                        );
                    } else {
                        info!(
                            "the Hub has no AcoustID key configured; track identification is off. \
                             Set ACOUSTID_API_KEY on the Hub, or [acoustid] api_key here to \
                             identify without one."
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, via = resolver.label(), "acoustid pass failed");
                    tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                }
            }
        }
    });
}

/// Marker carried by the error that retires the Hub route. A typed marker rather than a string match
/// so the "stop asking the Hub" decision cannot be broken by reworded log text.
#[derive(Debug, thiserror::Error)]
#[error("the Hub has no AcoustID key configured")]
struct NotConfigured;

/// One track awaiting identification: its current tags (to disambiguate the AcoustID candidates) and
/// a library and path (to fingerprint and, once enriched, organize on disk).
#[derive(sqlx::FromRow)]
struct PendingRow {
    id: String,
    title: String,
    album: String,
    album_id: Option<String>,
    /// Needed to CREATE an album when the track has none; `albums.artist_id` is not nullable.
    artist_id: Option<String>,
    library_id: String,
    path: String,
}

/// Identify up to `BATCH` tracks that have no AcoustID yet, storing the authoritative recording id,
/// track/disc position, and album info (which the metadata organize/dedupe need), then placing the
/// file on disk now that it's complete. Returns how many were resolved.
async fn identify_batch(state: &AppState, resolver: &Resolver) -> anyhow::Result<u32> {
    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());

    let rows: Vec<PendingRow> = sqlx::query_as(
        "SELECT t.id, t.title, COALESCE(al.title, '') AS album, t.album_id, t.artist_id, \
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
        // Audio is read here and nowhere else. Everything past this line is a hash.
        let fp = match compute(Path::new(&r.path), &state.config.acoustid.fpcalc_path).await {
            Ok(fp) => fp,
            Err(e) => {
                warn!(track = %r.id, error = %e, "fpcalc failed - skipping");
                continue;
            }
        };
        tokio::time::sleep(REQ_SPACING).await;

        let outcome =
            match resolver {
                Resolver::Hub(creds) => {
                    let req = IdentifyRequest {
                        fingerprint: fp.fingerprint,
                        duration_ms: fp.duration_secs.saturating_mul(1000),
                        title: Some(r.title.clone()).filter(|t| !t.is_empty()),
                        artist: None,
                        album: Some(r.album.clone()).filter(|a| !a.is_empty()),
                    };
                    hub.identify(&creds.server_api_key, &req).await
                }
                Resolver::Local(key) => lookup(&state.http, key, &fp, &r.title, &r.album)
                    .await
                    .map(|id| match id {
                        Some(id) => IdentifyOutcome::Identified(Box::new(id)),
                        None => IdentifyOutcome::NoMatch,
                    }),
            };

        match outcome {
            Ok(IdentifyOutcome::Identified(identity)) => {
                apply_identity(state, &r, &identity).await?;
                resolved += 1;
            }
            Ok(IdentifyOutcome::NoMatch) => { /* leave NULL and retry on a much later run */ }
            // Abort the whole pass, not just this track: the answer is a property of the Hub, so
            // every remaining track in the batch would get it too.
            Ok(IdentifyOutcome::NotConfigured) => return Err(NotConfigured.into()),
            // Never swallowed into "no match" — the library has to know this is worth retrying.
            Err(e) => warn!(track = %r.id, error = %e, "identify request failed"),
        }
    }
    Ok(resolved)
}

/// Resolve a fingerprint to an identity by calling AcoustID directly — the standalone path, used
/// when there is no Hub to ask.
///
/// Requests rich meta so the response carries the recording's releases and this track's position on
/// them, which is what lets an untagged import gain an album and a track number.
async fn lookup(
    http: &reqwest::Client,
    api_key: &str,
    fp: &Fingerprint,
    title: &str,
    album: &str,
) -> anyhow::Result<Option<IdentifyResponse>> {
    let duration = fp.duration_secs.to_string();
    let url = reqwest::Url::parse_with_params(
        "https://api.acoustid.org/v2/lookup",
        &[
            ("client", api_key),
            ("duration", duration.as_str()),
            ("fingerprint", fp.fingerprint.as_str()),
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

/// True when a release's title normalizes to the wanted album (and the wanted album isn't blank).
fn release_matches_album(rel: &serde_json::Value, album_norm: &str) -> bool {
    !album_norm.is_empty()
        && rel["title"]
            .as_str()
            .is_some_and(|t| normalize(t) == album_norm)
}

/// A recording has a release matching the wanted album.
fn recording_matches_album(rec: &serde_json::Value, album_norm: &str) -> bool {
    rec["releases"].as_array().is_some_and(|rels| {
        rels.iter()
            .any(|rel| release_matches_album(rel, album_norm))
    })
}

/// A recording's title normalizes to the wanted title.
fn recording_matches_title(rec: &serde_json::Value, title_norm: &str) -> bool {
    !title_norm.is_empty()
        && rec["title"]
            .as_str()
            .is_some_and(|t| normalize(t) == title_norm)
}

/// Extract `(disc_no, track_no)` from the first medium of a release that lists this recording's
/// track (AcoustID nests just our track under each medium).
fn release_position(rel: &serde_json::Value) -> (Option<u16>, Option<u16>) {
    let small = |v: &serde_json::Value| v.as_u64().and_then(|n| u16::try_from(n).ok());
    for m in rel["mediums"].as_array().into_iter().flatten() {
        if let Some(track) = m["tracks"].as_array().and_then(|ts| ts.first()) {
            let track_no = small(&track["position"]);
            if track_no.is_some() {
                return (small(&m["position"]), track_no);
            }
        }
    }
    (None, None)
}

/// Parse an AcoustID `v2/lookup` (rich meta) response into an [`IdentifyResponse`].
///
/// From the highest-scoring result it picks the recording that best matches the file being
/// identified, preferring one whose release matches the file's album tag — this is what makes two
/// encodings of the same track on the same album converge on one `recording_mbid` — then a title
/// match, then the first. Pure, so the ranking is testable.
///
/// The Hub runs an equivalent parse in `api/v1/identify.rs`, so the two routes agree on what a
/// response means. They are separate on purpose: the library must be able to do this with no Hub in
/// the picture at all. The tests below pin the ranking on both sides of that split.
fn parse_lookup(
    body: &serde_json::Value,
    want_title_norm: &str,
    want_album_norm: &str,
) -> Option<IdentifyResponse> {
    if body["status"].as_str() != Some("ok") {
        return None;
    }
    let results = body["results"].as_array()?;
    let best = results.iter().max_by(|a, b| {
        let score = |v: &serde_json::Value| v["score"].as_f64().unwrap_or(0.0);
        score(a)
            .partial_cmp(&score(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let acoustid = best["id"].as_str()?.to_string();

    let empty = Vec::new();
    let recordings = best["recordings"].as_array().unwrap_or(&empty);

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

    let mut id = IdentifyResponse {
        acoustid,
        recording_mbid: None,
        album: None,
        release_mbid: None,
        album_artist: None,
        title: None,
        track_no: None,
        disc_no: None,
        year: None,
    };
    if let Some(rec) = rec {
        id.recording_mbid = rec["id"].as_str().map(String::from);
        id.title = rec["title"].as_str().map(String::from);
        // Prefer the release matching the album tag; else the first listed.
        let rel = rec["releases"].as_array().and_then(|rels| {
            rels.iter()
                .find(|rel| release_matches_album(rel, want_album_norm))
                .or_else(|| rels.first())
        });
        if let Some(rel) = rel {
            id.album = rel["title"].as_str().map(String::from);
            id.release_mbid = rel["id"].as_str().map(String::from);
            // A year outside u16 is dropped rather than truncated into a plausible wrong answer,
            // matching what the Hub does with the same field.
            id.year = rel["date"]["year"]
                .as_u64()
                .and_then(|y| u16::try_from(y).ok());
            let (disc, track) = release_position(rel);
            id.disc_no = disc;
            id.track_no = track;
        }
    }
    Some(id)
}

/// Write a resolved identity onto the track (and its album), then place the file on disk.
async fn apply_identity(
    state: &AppState,
    r: &PendingRow,
    identity: &IdentifyResponse,
) -> anyhow::Result<()> {
    let year = identity.year.map(i64::from);
    let track_no = identity.track_no.map(i64::from);
    let disc_no = identity.disc_no.map(i64::from);

    // recording_mbid is overwritten (the album/title-matched pick is more reliable than any prior
    // guess); track/disc backfill only where the file tags lack them.
    sqlx::query(
        "UPDATE tracks SET acoustid = ?, \
         recording_mbid = COALESCE(?, recording_mbid), \
         track_no = COALESCE(track_no, ?), disc_no = COALESCE(disc_no, ?) WHERE id = ?",
    )
    .bind(&identity.acoustid)
    .bind(identity.recording_mbid.as_deref())
    .bind(track_no)
    .bind(disc_no)
    .bind(&r.id)
    .execute(&state.db)
    .await?;

    // Backfill album-level facts (year / release id) onto the album.
    // The album the fingerprint resolved is only useful to a track that HAS NONE — which is exactly
    // the branch this used to skip. Create it, then fall through to the same backfill so year and
    // release id land on it either way.
    let album_id = match (&r.album_id, &r.artist_id, identity.album.as_deref()) {
        (None, Some(artist_id), Some(album)) => match crate::index::attach_album_from_identity(
            &state.db,
            &r.id,
            artist_id,
            album,
            year,
            identity.release_mbid.as_deref(),
        )
        .await
        {
            Ok(id) => {
                if id.is_some() {
                    info!(track = %r.id, album, "identified an album for an untagged track");
                }
                id
            }
            Err(e) => {
                warn!(track = %r.id, error = %e, "attaching the identified album failed");
                None
            }
        },
        _ => r.album_id.clone(),
    };

    if let Some(album_id) = &album_id {
        sqlx::query(
            "UPDATE albums SET year = COALESCE(year, ?), \
             release_mbid = COALESCE(release_mbid, ?) WHERE id = ?",
        )
        .bind(year)
        .bind(identity.release_mbid.as_deref())
        .bind(album_id)
        .execute(&state.db)
        .await?;
    }

    // Now that the track has its real metadata, place it on disk (organize was gated earlier when
    // the track number was missing).
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
    Ok(())
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

    /// A year AcoustID reports outside `u16` is dropped, not truncated. A wrong-but-plausible year
    /// is worse than none: it silently sorts an album into the wrong era forever.
    #[test]
    fn absurd_years_are_dropped_rather_than_truncated() {
        let body = serde_json::json!({
            "status": "ok",
            "results": [{ "id": "a", "score": 1.0, "recordings": [{ "id": "r",
                "releases": [{ "title": "X", "id": "rel", "date": {"year": 70000} }] }] }]
        });
        assert_eq!(parse_lookup(&body, "", "").unwrap().year, None);
    }
}
