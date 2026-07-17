//! Catalog sync: push this library's metadata + embedded artwork to the Central Hub.
//!
//! When `metadata_storage = "hub"` (the default), a background loop periodically uploads the
//! catalog of every Hub-linked library. The Hub derives canonical artists/albums, enriches them
//! from external providers, and serves browsing. Cover bytes are only uploaded when the Hub reports
//! it is missing them, so steady-state syncs are cheap.

use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chordia_contracts::catalog::{
    AlbumArtistResolution, CatalogPruneRequest, CatalogSyncRequest, SyncTrack,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::MetadataStorage;
use crate::http::AppState;
use crate::pairing::HubClient;

/// Tracks per sync request, kept so the JSON body stays well under the Hub's body limit.
const BATCH: usize = 500;
const SYNC_INTERVAL_SECS: u64 = 180;

/// Re-push everything at least this often even when the payload is unchanged. The sync RESPONSE
/// (missing covers, canonical album-artist resolutions) is how the Hub reconciles with this
/// library, so skipped pushes must not starve that forever — and a periodic full push also heals
/// Hub-side drift (e.g. a restored backup) the hash can't see.
const FORCE_SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Last successfully-pushed payload hash + when, per local library id. In-memory on purpose: a
/// fresh process always starts with one full sync, which doubles as the startup safety net.
static SYNC_STATE: LazyLock<Mutex<HashMap<String, (u64, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(sqlx::FromRow)]
struct SyncRow {
    id: String,
    title: String,
    artist: String,
    artist_norm: String,
    album: Option<String>,
    album_norm: Option<String>,
    album_artist: Option<String>,
    track_no: Option<i64>,
    disc_no: Option<i64>,
    year: Option<i64>,
    genre: Option<String>,
    duration_ms: i64,
    content_hash: String,
    recording_mbid: Option<String>,
    release_mbid: Option<String>,
    isrc: Option<String>,
    cover_hash: Option<String>,
    edition: Option<String>,
    advisory: Option<String>,
}

fn row_to_track(r: SyncRow) -> SyncTrack {
    SyncTrack {
        title: r.title,
        artist: r.artist,
        artist_normalized: r.artist_norm,
        album: r.album,
        album_normalized: r.album_norm,
        album_artist: r.album_artist,
        track_no: r.track_no.map(|n| n as u16),
        disc_no: r.disc_no.map(|n| n as u16),
        year: r.year.map(|y| y as u16),
        genre: r.genre,
        duration_ms: r.duration_ms as u32,
        track_ref: r.id,
        content_hash: r.content_hash,
        recording_mbid: r.recording_mbid,
        release_mbid: r.release_mbid,
        isrc: r.isrc,
        cover_hash: r.cover_hash,
        edition: r.edition,
        advisory: r.advisory,
    }
}

/// Sync every Hub-linked library once. No-op if storage is local or the server isn't paired.
pub async fn sync_all(state: &AppState) -> anyhow::Result<()> {
    if state.config.metadata_storage != MetadataStorage::Hub {
        return Ok(());
    }
    let Some(creds) = state.credentials.read().await.clone() else {
        return Ok(());
    };

    let hub = HubClient::new(state.config.backend_url.clone(), state.http.clone());
    let libs: Vec<(String, String)> =
        sqlx::query_as("SELECT id, hub_library_id FROM libraries WHERE hub_library_id IS NOT NULL")
            .fetch_all(&state.db)
            .await?;

    for (local_id, hub_id) in libs {
        if let Err(e) = sync_library(state, &hub, &creds.server_api_key, &local_id, &hub_id).await {
            warn!(library = %local_id, error = %e, "catalog sync failed");
        }
    }
    Ok(())
}

async fn sync_library(
    state: &AppState,
    hub: &HubClient,
    api_key: &str,
    local_id: &str,
    hub_id: &str,
) -> anyhow::Result<()> {
    let hub_uuid: Uuid = hub_id.parse()?;

    // Rebuild the flat wire shape from the normalized tables. `artist_id IS NOT NULL` skips any row
    // a re-scan hasn't relinked yet, so we never push an incomplete credit to the Hub.
    let rows: Vec<SyncRow> = sqlx::query_as(
        "SELECT t.id, t.title, COALESCE(ar.name, '') AS artist, \
                COALESCE(ar.name_normalized, '') AS artist_norm, \
                al.title AS album, al.title_normalized AS album_norm, aa.name AS album_artist, \
                t.track_no, t.disc_no, al.year AS year, al.genre AS genre, t.duration_ms, \
                t.content_hash, t.recording_mbid, al.release_mbid AS release_mbid, t.isrc, \
                t.cover_hash, t.edition, t.advisory \
         FROM library_tracks lt JOIN tracks t ON t.id = lt.track_id \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         LEFT JOIN artists aa ON aa.id = al.artist_id \
         WHERE lt.library_id = ? AND t.artist_id IS NOT NULL \
         ORDER BY t.id",
    )
    .bind(local_id)
    .fetch_all(&state.db)
    .await?;

    // The authoritative ref set for reconciliation, collected before we consume `rows`. An empty
    // library still runs the prune below so its last deletions propagate.
    let track_refs: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();

    let tracks: Vec<SyncTrack> = rows.into_iter().map(row_to_track).collect();

    // Skip the push entirely when nothing changed since the last successful cycle (the ORDER BY
    // above makes the serialized payload — and so this hash — deterministic). This is what keeps a
    // steady-state library from re-uploading its whole catalog every 3 minutes; the forced
    // interval below bounds how long Hub-side reconciliation can be deferred.
    let payload_hash = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write(&serde_json::to_vec(&tracks).unwrap_or_default());
        h.finish()
    };
    if let Ok(st) = SYNC_STATE.lock() {
        if let Some((prev, at)) = st.get(local_id) {
            if *prev == payload_hash && at.elapsed() < FORCE_SYNC_INTERVAL {
                debug!(library = %local_id, "catalog unchanged; skipping sync push");
                return Ok(());
            }
        }
    }

    let mut missing: HashSet<String> = HashSet::new();
    let mut resolutions: Vec<AlbumArtistResolution> = Vec::new();
    for chunk in tracks.chunks(BATCH) {
        let resp = hub
            .sync_catalog(
                api_key,
                &CatalogSyncRequest {
                    library_id: hub_uuid,
                    tracks: chunk.to_vec(),
                },
            )
            .await?;
        missing.extend(resp.missing_covers);
        resolutions.extend(resp.canonical_album_artists);
    }

    // Adopt the Hub's canonical album-artist for each album, and relocate the files of any album whose
    // folder name changed (consolidating e.g. "Machine Gun Kelly"/"MGK"/"Wiz Khalifa" → "mgk").
    apply_canonical_album_artists(state, local_id, &resolutions).await;

    // Upload only the artwork the Hub asked for.
    for hash in missing {
        if let Some((mime, bytes)) = sqlx::query_as::<_, (String, Vec<u8>)>(
            "SELECT mime, bytes FROM cover_art WHERE hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&state.db)
        .await?
        {
            if let Err(e) = hub.upload_cover(api_key, &hash, &mime, bytes).await {
                warn!(error = %e, "cover upload failed");
            }
        }
    }

    // Reconcile deletions: tell the Hub the full current ref set so it drops memberships for files
    // removed on this side, so deleted tracks leave browsing.
    let prune_ok = match hub
        .prune_catalog(
            api_key,
            &CatalogPruneRequest {
                library_id: hub_uuid,
                track_refs,
            },
        )
        .await
    {
        Ok(_) => true,
        Err(e) => {
            warn!(error = %e, "catalog prune failed");
            false
        }
    };

    // Record the hash only when the WHOLE cycle (including prune) landed, so a partial failure is
    // retried on the next 3-minute tick instead of being skipped for up to an hour.
    if prune_ok {
        if let Ok(mut st) = SYNC_STATE.lock() {
            st.insert(local_id.to_string(), (payload_hash, Instant::now()));
        }
    }

    info!(library = %local_id, tracks = tracks.len(), "catalog synced to hub");
    Ok(())
}

/// Store the Hub's canonical album-artist on each matching album, and relocate the files of any album
/// whose folder name changed. Matches the library's album by its normalized title + the album-artist
/// name we sent (so it pairs to exactly the row the Hub resolved). Best-effort: a failed match/move is
/// logged, never fatal to the sync.
async fn apply_canonical_album_artists(
    state: &AppState,
    local_id: &str,
    resolutions: &[AlbumArtistResolution],
) {
    if resolutions.is_empty() {
        return;
    }
    let mut changed: Vec<String> = Vec::new();
    for r in resolutions {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT al.id, al.canonical_album_artist FROM albums al \
             JOIN artists ar ON ar.id = al.artist_id \
             WHERE al.title_normalized = ?1 AND ar.name = ?2 LIMIT 1",
        )
        .bind(&r.album_normalized)
        .bind(&r.album_artist)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        let Some((album_id, current)) = row else {
            continue;
        };
        if current.as_deref() == Some(r.canonical_artist.as_str()) {
            continue; // already up to date
        }
        if sqlx::query("UPDATE albums SET canonical_album_artist = ?1 WHERE id = ?2")
            .bind(&r.canonical_artist)
            .bind(&album_id)
            .execute(&state.db)
            .await
            .is_ok()
        {
            changed.push(album_id);
        }
    }
    if changed.is_empty() {
        return;
    }
    // Only relocate when on-disk organization is enabled; otherwise the names are stored and files move
    // whenever organize is turned on (reorganize_library) or the album is next touched.
    let Some((root, settings)) = crate::organize::library_settings(&state.db, local_id).await
    else {
        return;
    };
    for album_id in &changed {
        if let Err(e) =
            crate::organize::reorganize_album(&state.db, local_id, &root, &settings, album_id).await
        {
            warn!(album_id = %album_id, error = %e, "relocate after album-artist change failed");
        }
    }
    info!(library = %local_id, albums = changed.len(), "relocated albums to canonical artist folder");
}

/// Spawn the background sync loop.
pub fn start_sync_loop(state: AppState) {
    tokio::spawn(async move {
        // Give initial scans a head start before the first push.
        tokio::time::sleep(Duration::from_secs(10)).await;
        loop {
            if let Err(e) = sync_all(&state).await {
                warn!(error = %e, "catalog sync loop error");
            }
            tokio::time::sleep(Duration::from_secs(SYNC_INTERVAL_SECS)).await;
        }
    });
}
