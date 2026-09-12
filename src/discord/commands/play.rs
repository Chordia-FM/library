//! `/play`, `/search`, `/album`, `/artist` — getting music into the queue.

use std::sync::Arc;

use serenity::all::AutocompleteChoice;
use sqlx::SqlitePool;
use uuid::Uuid;

use super::{guard, Context, Error};
use crate::catalog::{self, TrackRow};
use crate::discord::hub;
use crate::discord::player::{Cover, Position, QueueItem};
use crate::discord::ui::{fmt, send, views};
use crate::error::AppResult;
use crate::http::AppState;
use crate::search::{self, HitKind, SearchHit};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum PlayPosition {
    #[name = "last"]
    Last,
    #[name = "next"]
    Next,
}

impl From<Option<PlayPosition>> for Position {
    fn from(p: Option<PlayPosition>) -> Self {
        match p {
            Some(PlayPosition::Next) => Position::Next,
            _ => Position::Last,
        }
    }
}

/// What a query resolved to: the tracks to queue, and what to call the set when it is more than one.
pub struct Resolved {
    pub tracks: Vec<Arc<TrackRow>>,
    /// What the query was: a track, an album, an artist or a playlist. The toast is laid out by
    /// it.
    pub kind: HitKind,
    pub source: Option<String>,
    /// The source's page on the web client, relative to it (a playlist's, say).
    pub source_url: Option<String>,
    /// Set when the query was an artist, so the toast can show their picture rather than the
    /// first album's cover.
    pub artist: Option<ArtistRef>,
}

impl Resolved {
    /// Nothing matched.
    fn none(kind: HitKind) -> Self {
        Resolved {
            tracks: Vec::new(),
            kind,
            source: None,
            source_url: None,
            artist: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtistRef {
    pub name_normalized: String,
    pub mbid: Option<String>,
}

/// An artist's pictures from the Hub as attachments: the portrait and the banner, either of
/// which the Hub may not have.
#[derive(Default)]
pub struct ArtistPictures {
    pub image: Option<Cover>,
    pub banner: Option<Cover>,
}

/// The artist's pictures, when the query was an artist the Hub knows.
pub async fn art_for(state: &crate::http::AppState, resolved: &Resolved) -> ArtistPictures {
    let Some(a) = resolved.artist.as_ref() else {
        return ArtistPictures::default();
    };
    let Some(art) =
        crate::discord::hub::artist_art(state, &a.name_normalized, a.mbid.as_deref()).await
    else {
        return ArtistPictures::default();
    };
    let (image, banner) = crate::discord::hub::artist_pictures(state, &art).await;
    ArtistPictures { image, banner }
}

/// How many tracks an artist hit queues at most.
const ARTIST_CAP: i64 = 100;

/// Resolve `t:<id>` / `al:<id>` / `ar:<id>` (from autocomplete or a picker), or free text via the
/// best search hit of the allowed kinds.
pub async fn resolve(
    state: &AppState,
    query: &str,
    kinds: &[HitKind],
    by: serenity::all::UserId,
) -> AppResult<Resolved> {
    let db = &state.db;
    let q = query.trim();
    if let Some(id) = q.strip_prefix("pl:") {
        return resolve_playlist(state, id, by).await;
    }
    // Playlists live on the Hub, not in the library's index: free text asks it by name, as the
    // person asking, so their own playlists count.
    if kinds == [HitKind::Playlist] {
        return match hub::search_playlists(state, q, by.get())
            .await
            .into_iter()
            .next()
        {
            Some(p) => resolve_playlist(state, &p.id.to_string(), by).await,
            None => Ok(Resolved::none(HitKind::Playlist)),
        };
    }
    if let Some(id) = q.strip_prefix("t:") {
        return Ok(Resolved {
            tracks: catalog::get_track_row(db, id)
                .await?
                .map(Arc::new)
                .into_iter()
                .collect(),
            kind: HitKind::Track,
            source_url: None,
            source: None,
            artist: None,
        });
    }
    if let Some(id) = q.strip_prefix("al:") {
        return resolve_album(db, id).await;
    }
    if let Some(id) = q.strip_prefix("ar:") {
        return resolve_artist(db, id).await;
    }
    let hits = search::search(db, q, kinds, 1).await?;
    match hits.first() {
        None => Ok(Resolved {
            tracks: Vec::new(),
            kind: HitKind::Track,
            source_url: None,
            source: None,
            artist: None,
        }),
        Some(hit) => resolve_hit(state, hit, by).await,
    }
}

pub async fn resolve_hit(
    state: &AppState,
    hit: &SearchHit,
    by: serenity::all::UserId,
) -> AppResult<Resolved> {
    let db = &state.db;
    match hit.kind {
        HitKind::Track => Ok(Resolved {
            tracks: catalog::get_track_row(db, &hit.id)
                .await?
                .map(Arc::new)
                .into_iter()
                .collect(),
            kind: HitKind::Track,
            source_url: None,
            source: None,
            artist: None,
        }),
        HitKind::Album => resolve_album(db, &hit.id).await,
        HitKind::Artist => resolve_artist(db, &hit.id).await,
        HitKind::Playlist => resolve_playlist(state, &hit.id, by).await,
    }
}

/// A Chordia playlist by its Hub id: the tracks this library holds, in order, named and linked
/// to its page. Asked for as `by`, whose own playlists the Hub hands out.
async fn resolve_playlist(
    state: &AppState,
    id: &str,
    by: serenity::all::UserId,
) -> AppResult<Resolved> {
    let Ok(uuid) = id.parse::<Uuid>() else {
        return Ok(Resolved::none(HitKind::Playlist));
    };
    let Some((info, rows)) = hub::playlist_tracks(state, uuid, by.get()).await else {
        return Ok(Resolved::none(HitKind::Playlist));
    };
    Ok(Resolved {
        tracks: rows.into_iter().map(Arc::new).collect(),
        kind: HitKind::Playlist,
        source: Some(info.name),
        source_url: Some(format!("/app/playlists/{uuid}")),
        artist: None,
    })
}

async fn resolve_album(db: &SqlitePool, id: &str) -> AppResult<Resolved> {
    let tracks = search::album_tracks(db, id).await?;
    let source = tracks.first().and_then(|t| t.album.clone());
    Ok(Resolved {
        tracks: tracks.into_iter().map(Arc::new).collect(),
        kind: HitKind::Album,
        source_url: None,
        source,
        artist: None,
    })
}

async fn resolve_artist(db: &SqlitePool, id: &str) -> AppResult<Resolved> {
    let tracks = search::artist_tracks(db, id, ARTIST_CAP).await?;
    let source = tracks.first().map(|t| t.artist.clone());
    let artist = match tracks.first() {
        Some(t) => {
            let mbid =
                sqlx::query_scalar::<_, Option<String>>("SELECT mbid FROM artists WHERE id = ?")
                    .bind(id)
                    .fetch_optional(db)
                    .await?
                    .flatten();
            Some(ArtistRef {
                name_normalized: t.artist_norm.clone(),
                mbid,
            })
        }
        None => None,
    };
    Ok(Resolved {
        tracks: tracks.into_iter().map(Arc::new).collect(),
        kind: HitKind::Artist,
        source_url: None,
        source,
        artist,
    })
}

/// Autocomplete for `/play`: tracks, albums and artists, labelled by kind, valued by prefixed id.
async fn autocomplete_query(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    autocomplete_kinds(
        ctx,
        partial,
        &[HitKind::Track, HitKind::Album, HitKind::Artist],
    )
    .await
}

async fn autocomplete_album(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    autocomplete_kinds(ctx, partial, &[HitKind::Album]).await
}

async fn autocomplete_artist(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    autocomplete_kinds(ctx, partial, &[HitKind::Artist]).await
}

/// Autocomplete for `/playlist`: the Hub's playlists the asker may queue, by name, their own
/// first; with nothing typed, their own and the newest public ones.
async fn autocomplete_playlist(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    if !super::serves(ctx) {
        return Vec::new();
    }
    hub::search_playlists(&ctx.data().state, partial, ctx.author().id.get())
        .await
        .into_iter()
        .map(|p| {
            let whose = if p.owned {
                "yours".to_string()
            } else {
                format!("by {}", p.owner_handle)
            };
            let label = format!(
                "📃 {} · {} · {whose}",
                p.name,
                fmt::count(p.track_count as usize, "track")
            );
            AutocompleteChoice::new(fmt::ellipsize(&label, 100), format!("pl:{}", p.id))
        })
        .collect()
}

async fn autocomplete_kinds(
    ctx: Context<'_>,
    partial: &str,
    kinds: &[HitKind],
) -> Vec<AutocompleteChoice> {
    // A suggestion list is a catalog read; commands are checked, so these are too.
    if !super::serves(ctx) {
        return Vec::new();
    }
    let identity = ctx.data();
    let db = &identity.state.db;
    // Nothing typed yet: the asker's own recent requests, what this server plays most, and what
    // is newest, rather than an empty list.
    let hits = if partial.trim().is_empty() {
        let Some(guild) = ctx.guild_id() else {
            return Vec::new();
        };
        let app_id = identity.app_id_sync().unwrap_or(0).to_string();
        let user = ctx.author().id.get();
        crate::discord::suggest::suggest(db, &app_id, &guild.get().to_string(), user, kinds, 25)
            .await
    } else {
        search::search(db, partial, kinds, 25).await
    };
    let hits = match hits {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(error = %e, "autocomplete failed");
            return Vec::new();
        }
    };
    hits.into_iter()
        .map(|h| {
            let (prefix, label) = match h.kind {
                HitKind::Track => ("t", format!("{} · {}", h.title, h.subtitle)),
                HitKind::Album => ("al", format!("💿 {} · {}", h.title, h.subtitle)),
                HitKind::Artist => ("ar", format!("👤 {} · {}", h.title, h.subtitle)),
                HitKind::Playlist => ("pl", format!("📃 {} · {}", h.title, h.subtitle)),
            };
            AutocompleteChoice::new(fmt::ellipsize(&label, 100), format!("{prefix}:{}", h.id))
        })
        .collect()
}

/// Shared tail of every "queue something" command.
async fn queue_resolved(
    ctx: Context<'_>,
    query: &str,
    kinds: &[HitKind],
    position: Position,
) -> Result<(), Error> {
    let identity = ctx.data();
    let (vc, player) = match guard::listener(ctx).await {
        Ok(x) => x,
        Err(r) => return send::respond(ctx, r.view(&super::snap(ctx).await)).await,
    };
    let resolved = resolve(&identity.state, query, kinds, ctx.author().id).await?;
    if resolved.tracks.is_empty() {
        let what = match kinds {
            [HitKind::Album] => "No album",
            [HitKind::Artist] => "No artist",
            [HitKind::Playlist] => "No playlist you can queue",
            _ => "Nothing",
        };
        return send::respond(
            ctx,
            views::notice(
                &super::snap(ctx).await,
                &format!("{what} in the library matches"),
                &format!(
                    "-# “{}”. Try `/search` for a wider look.",
                    fmt::escape_md(&fmt::ellipsize(query, 80))
                ),
            ),
        )
        .await;
    }
    if player.voice_channel().await != Some(vc) {
        if let Err(e) = player.join(vc, ctx.channel_id()).await {
            return send::respond(
                ctx,
                views::error(&super::snap(ctx).await, "Couldn't join", &format!("-# {e}")),
            )
            .await;
        }
    }
    // An artist's own picture beats the first album's cover, when the Hub has one.
    let art = art_for(&identity.state, &resolved).await;
    let items: Vec<QueueItem> = resolved
        .tracks
        .into_iter()
        .map(|track| QueueItem {
            track,
            requested_by: ctx.author().id,
            autoplay: false,
        })
        .collect();
    let cover = match &art.image {
        Some(a) => Some(a.clone()),
        None => Cover::load(&identity.state.db, &items[0].track).await,
    };
    let enq = player.enqueue(items.clone(), position).await?;
    let snap = player.snapshot().await;
    send::respond(
        ctx,
        views::queued(
            &snap,
            &items,
            &enq,
            views::Added {
                kind: resolved.kind,
                source: resolved.source.as_deref(),
                url: resolved.source_url.as_deref(),
            },
            cover.as_ref(),
            art.image.as_ref(),
            art.banner.as_ref(),
        ),
    )
    .await
}

/// Play a track, album or artist from the library
#[poise::command(slash_command, guild_only)]
pub async fn play(
    ctx: Context<'_>,
    #[description = "Track, album or artist"]
    #[autocomplete = "autocomplete_query"]
    query: String,
    #[description = "Add to the end of the queue (default) or play next"] position: Option<
        PlayPosition,
    >,
) -> Result<(), Error> {
    ctx.defer().await?;
    queue_resolved(
        ctx,
        &query,
        &[HitKind::Track, HitKind::Album, HitKind::Artist],
        position.into(),
    )
    .await
}

/// Queue a whole album
#[poise::command(slash_command, guild_only)]
pub async fn album(
    ctx: Context<'_>,
    #[description = "Album title"]
    #[autocomplete = "autocomplete_album"]
    query: String,
    #[description = "Add to the end of the queue (default) or play next"] position: Option<
        PlayPosition,
    >,
) -> Result<(), Error> {
    ctx.defer().await?;
    queue_resolved(ctx, &query, &[HitKind::Album], position.into()).await
}

/// Queue one of your Chordia playlists, or a public one
#[poise::command(slash_command, guild_only)]
pub async fn playlist(
    ctx: Context<'_>,
    #[description = "Playlist name"]
    #[autocomplete = "autocomplete_playlist"]
    query: String,
    #[description = "Add to the end of the queue (default) or play next"] position: Option<
        PlayPosition,
    >,
) -> Result<(), Error> {
    ctx.defer().await?;
    queue_resolved(ctx, &query, &[HitKind::Playlist], position.into()).await
}

/// Queue an artist's tracks, album by album
#[poise::command(slash_command, guild_only)]
pub async fn artist(
    ctx: Context<'_>,
    #[description = "Artist name"]
    #[autocomplete = "autocomplete_artist"]
    query: String,
    #[description = "Add to the end of the queue (default) or play next"] position: Option<
        PlayPosition,
    >,
) -> Result<(), Error> {
    ctx.defer().await?;
    queue_resolved(ctx, &query, &[HitKind::Artist], position.into()).await
}

/// Search the library and pick what to play
#[poise::command(slash_command, guild_only)]
pub async fn search(
    ctx: Context<'_>,
    #[description = "What to look for"] query: String,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let identity = ctx.data();
    let guild = super::guild_of(ctx)?;
    let hits = search::search(
        &identity.state.db,
        &query,
        &[HitKind::Track, HitKind::Album, HitKind::Artist],
        10,
    )
    .await?;
    send::respond(
        ctx,
        views::search_results(
            &identity.icons(),
            identity.index,
            guild.get(),
            &query,
            &hits,
        ),
    )
    .await
}
