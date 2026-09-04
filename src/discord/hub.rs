//! What the bot asks the Hub for, and remembers: which listeners are Chordia users (for the
//! controller's "counting for" line; the reporter resolves again when it sends), where a track's
//! page is, and what an artist looks like. Every answer is cached in the [`Runtime`], negative
//! answers too, so a busy channel costs the Hub a handful of requests an hour, not one per edit.
//!
//! Without a Hub (or before pairing) every lookup is `None` and the bot simply shows less.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chordia_contracts::discord::{
    ArtistArt, ArtistArtRequest, ListenersNowPlaying, ResolveListenersRequest,
    ResolveTracksRequest, ResolvedListener, ResolvedTrack,
};
use chordia_contracts::social::NowPlayingReport;
use uuid::Uuid;

use crate::catalog::TrackRow;
use crate::http::AppState;
use crate::pairing::HubClient;

/// How long a listener answer stands. Short: a share can be granted or an opt-out flipped.
const LISTENER_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a track's or artist's page answer stands.
const LINK_TTL: Duration = Duration::from_secs(24 * 60 * 60);

type Cached<T> = Mutex<HashMap<String, (Instant, Option<T>)>>;

#[derive(Default)]
pub struct Caches {
    listeners: Cached<ResolvedListener>,
    tracks: Cached<ResolvedTrack>,
    artists: Cached<ArtistArt>,
}

fn caches() -> Option<std::sync::Arc<super::Runtime>> {
    super::runtime()
}

fn fresh<T: Clone>(cache: &Cached<T>, key: &str, ttl: Duration) -> Option<Option<T>> {
    let map = cache.lock().unwrap_or_else(|e| e.into_inner());
    map.get(key)
        .filter(|(at, _)| at.elapsed() < ttl)
        .map(|(_, v)| v.clone())
}

fn remember<T>(cache: &Cached<T>, key: String, value: Option<T>) {
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, (Instant::now(), value));
}

async fn key(state: &AppState) -> Option<String> {
    state
        .credentials
        .read()
        .await
        .as_ref()
        .map(|c| c.server_api_key.clone())
}

fn hub(state: &AppState) -> HubClient {
    HubClient::new(state.config.backend_url.clone(), state.http.clone())
}

/// A Hub-relative URL (`/v1/images/…`) as something Discord can fetch.
pub fn absolute(state: &AppState, rel: &str) -> Option<String> {
    let base = state.config.backend_url.as_deref()?.trim_end_matches('/');
    Some(format!("{base}{rel}"))
}

/// The Chordia users among these listeners, from the cache; the ones it does not know are asked
/// for. Absent means: not a user this server may count for, or not known yet.
pub async fn resolve_listeners(state: &AppState, ids: &[u64]) -> HashMap<u64, ResolvedListener> {
    let Some(rt) = caches() else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    let mut ask: Vec<String> = Vec::new();
    for id in ids {
        match fresh(&rt.hub.listeners, &id.to_string(), LISTENER_TTL) {
            Some(Some(l)) => {
                out.insert(*id, l);
            }
            Some(None) => {}
            None => ask.push(id.to_string()),
        }
    }
    if ask.is_empty() {
        return out;
    }
    let Some(key) = key(state).await else {
        return out;
    };
    let req = ResolveListenersRequest {
        discord_ids: ask.clone(),
    };
    match hub(state).resolve_listeners(&key, &req).await {
        Ok(resp) => {
            let mut found: HashMap<String, ResolvedListener> = resp
                .listeners
                .into_iter()
                .map(|l| (l.discord_id.clone(), l))
                .collect();
            for id in ask {
                let value = found.remove(&id);
                if let (Some(l), Ok(n)) = (&value, id.parse::<u64>()) {
                    out.insert(n, l.clone());
                }
                remember(&rt.hub.listeners, id, value);
            }
        }
        Err(e) => tracing::debug!(error = %e, "resolving Discord listeners"),
    }
    out
}

/// What the cache already knows about these listeners, without asking. For a view.
pub fn cached_listeners(ids: &[u64]) -> Vec<ResolvedListener> {
    let Some(rt) = caches() else {
        return Vec::new();
    };
    let mut out: Vec<ResolvedListener> = ids
        .iter()
        .filter_map(|id| fresh(&rt.hub.listeners, &id.to_string(), LISTENER_TTL).flatten())
        .collect();
    out.sort_by(|a, b| a.handle.cmp(&b.handle));
    out
}

/// Tell the Hub what these listeners are hearing right now, or (with no report) that it stopped,
/// so their profiles show it. Best effort: a Hub that is away simply shows nothing.
pub async fn now_playing(state: &AppState, user_ids: Vec<Uuid>, report: Option<NowPlayingReport>) {
    if user_ids.is_empty() {
        return;
    }
    let Some(key) = key(state).await else { return };
    let body = ListenersNowPlaying { user_ids, report };
    if let Err(e) = hub(state).listeners_now_playing(&key, &body).await {
        tracing::debug!(error = %e, "reporting listeners' now playing");
    }
}

/// The Hub's id for one of the library's own libraries.
pub async fn hub_library_id(state: &AppState, local_library_id: &str) -> Option<Uuid> {
    sqlx::query_scalar::<_, Option<String>>("SELECT hub_library_id FROM libraries WHERE id = ?")
        .bind(local_library_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .flatten()
        .and_then(|s| s.parse().ok())
}

/// Where a track's page is on the Hub, for a deep link.
pub async fn resolve_track(state: &AppState, track: &TrackRow) -> Option<ResolvedTrack> {
    let rt = caches()?;
    if let Some(hit) = fresh(&rt.hub.tracks, &track.id, LINK_TTL) {
        return hit;
    }
    let key = key(state).await?;
    let library_id = hub_library_id(state, &track.library_id).await?;
    let req = ResolveTracksRequest {
        library_id,
        track_refs: vec![track.id.clone()],
    };
    let value = match hub(state).resolve_tracks(&key, &req).await {
        Ok(resp) => resp.tracks.into_iter().next(),
        Err(e) => {
            tracing::debug!(error = %e, "resolving a track's Hub page");
            return None;
        }
    };
    remember(&rt.hub.tracks, track.id.clone(), value.clone());
    value
}

/// An artist's page and picture, by MusicBrainz id when the library has one, else by name.
pub async fn artist_art(
    state: &AppState,
    name_normalized: &str,
    mbid: Option<&str>,
) -> Option<ArtistArt> {
    let rt = caches()?;
    let cache_key = mbid
        .map(|m| format!("mbid:{m}"))
        .unwrap_or_else(|| format!("name:{name_normalized}"));
    if let Some(hit) = fresh(&rt.hub.artists, &cache_key, LINK_TTL) {
        return hit;
    }
    let key = key(state).await?;
    let req = ArtistArtRequest {
        mbids: mbid.map(|m| vec![m.to_string()]).unwrap_or_default(),
        names_normalized: vec![name_normalized.to_string()],
    };
    let value = match hub(state).artists_art(&key, &req).await {
        Ok(resp) => {
            let mut artists = resp.artists;
            // Prefer the MusicBrainz match; a name can be shared, an mbid cannot.
            artists.sort_by_key(|a| (mbid.is_some() && a.mbid.as_deref() != mbid) as u8);
            artists.into_iter().next()
        }
        Err(e) => {
            tracing::debug!(error = %e, "looking up artist art");
            return None;
        }
    };
    remember(&rt.hub.artists, cache_key, value.clone());
    value
}
