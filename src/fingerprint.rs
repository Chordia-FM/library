//! Acoustic fingerprinting and Hub-side identification.
//!
//! Two halves, split along the one boundary that matters:
//!
//! * [`compute`] shells out to `fpcalc` (Chromaprint) to turn a decoded track into a fingerprint.
//!   **This is the only half that touches audio, and it stays here, on the machine that owns the
//!   file.** The Hub never sees a sample.
//! * The lookup — fingerprint → AcoustID → MusicBrainz recording — is a `POST` to the Hub. What
//!   crosses is the Chromaprint string: a lossy, one-way hash of a few hundred bytes describing the
//!   track's coarse spectral shape over time. Nothing listenable can be reconstructed from it, which
//!   is why this call may cross a boundary audio never may.
//!
//! # Why the lookup moved
//!
//! It used to live here too, gated behind this library's own `[acoustid] api_key`. Essentially no
//! self-hoster sets one, so the feature was off by default for everyone: a Manager download of FLACs
//! carrying only ARTIST and TITLE imported 33 tracks with no album and no artwork, and not one of
//! them was ever fingerprinted, because the key that would have allowed it did not exist on that
//! instance or on effectively any other. The Hub already holds third-party provider credentials, a
//! shared rate limiter and a response cache, so it holds the AcoustID key too. **Identification now
//! needs no library-side configuration at all** — that is the entire point of the move.
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
use crate::pairing::{HubClient, IdentifyOutcome};

/// How many unidentified tracks to attempt per pass.
const BATCH: i64 = 25;
/// Idle wait when there's nothing to do (or identification isn't available).
const IDLE_SECS: u64 = 300;
/// Spacing between identify requests.
///
/// This used to pace *our* calls to AcoustID. It now paces our calls to the **Hub**, which spends a
/// single AcoustID key on behalf of every library paired to it — so the politeness is owed to the
/// Hub's shared budget rather than to our own. The Hub queues on its own gate regardless; keeping
/// the spacing here means one library's backlog does not arrive as a burst that makes every other
/// library's identification wait behind it.
const REQ_SPACING: Duration = Duration::from_millis(400);

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

/// Spawn the background identification pass.
///
/// Needs no configuration: it runs whenever this library is paired to a Hub and using Hub metadata
/// storage. If that Hub has no AcoustID key it says so once (`501`), and the loop **exits** rather
/// than re-asking a question the Hub can never answer.
pub fn start_identification(state: AppState) {
    tokio::spawn(async move {
        loop {
            // Identification is a Hub round-trip, so it needs a Hub: local-metadata or unpaired
            // libraries simply have nobody to ask. Re-checked each pass because pairing can complete
            // while we are running.
            if state.config.metadata_storage != MetadataStorage::Hub
                || state.credentials.read().await.is_none()
            {
                tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                continue;
            }

            match identify_batch(&state).await {
                Ok(0) => tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await,
                Ok(n) => info!(identified = n, "acoustid: identified tracks"),
                // Not an error: this Hub does not offer identification. Say it once, at `info` so it
                // is actually visible (telemetry filters below that), then stop — a loop that kept
                // asking would burn a request every five minutes forever for a guaranteed 501.
                Err(e) if e.is::<NotConfigured>() => {
                    state.identify_available.store(false, Ordering::Relaxed);
                    info!(
                        "the Hub has no AcoustID key configured; track identification is off. \
                         Set ACOUSTID_API_KEY on the Hub to enable it - nothing is needed here."
                    );
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "acoustid pass failed");
                    tokio::time::sleep(Duration::from_secs(IDLE_SECS)).await;
                }
            }
        }
    });
}

/// Marker carried by the error that stops the identification loop. A typed marker rather than a
/// string match so the "stop asking" decision cannot be broken by reworded log text.
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
async fn identify_batch(state: &AppState) -> anyhow::Result<u32> {
    let Some(creds) = state.credentials.read().await.clone() else {
        return Ok(0);
    };
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

        let req = IdentifyRequest {
            fingerprint: fp.fingerprint,
            duration_ms: fp.duration_secs.saturating_mul(1000),
            title: Some(r.title.clone()).filter(|t| !t.is_empty()),
            artist: None,
            album: Some(r.album.clone()).filter(|a| !a.is_empty()),
        };
        match hub.identify(&creds.server_api_key, &req).await {
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
