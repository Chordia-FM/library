//! `/play`, `/search`, `/album`, `/artist` — getting music into the queue.

use std::sync::Arc;

use serenity::all::AutocompleteChoice;
use sqlx::SqlitePool;

use super::{guard, Context, Error};
use crate::catalog::{self, TrackRow};
use crate::discord::player::{Cover, Position, QueueItem};
use crate::discord::ui::{fmt, send, views};
use crate::error::AppResult;
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
    pub source: Option<String>,
    /// Set when the query was an artist, so the toast can show their picture rather than the
    /// first album's cover.
    pub artist: Option<ArtistRef>,
}

#[derive(Debug, Clone)]
pub struct ArtistRef {
    pub name_normalized: String,
    pub mbid: Option<String>,
}

/// The artist's picture from the Hub as an attachment, when the query was an artist the Hub
/// knows and has a picture for.
pub async fn art_for(state: &crate::http::AppState, resolved: &Resolved) -> Option<Cover> {
    let a = resolved.artist.as_ref()?;
    let art = crate::discord::hub::artist_art(state, &a.name_normalized, a.mbid.as_deref()).await?;
    let rel = art.image_url?;
    let (mime, bytes) = crate::discord::hub::image(state, &rel).await?;
    Some(Cover::named(
        &format!("artist-{}", art.artist_id),
        &mime,
        bytes,
    ))
}

/// How many tracks an artist hit queues at most.
const ARTIST_CAP: i64 = 100;

/// Resolve `t:<id>` / `al:<id>` / `ar:<id>` (from autocomplete or a picker), or free text via the
/// best search hit of the allowed kinds.
pub async fn resolve(db: &SqlitePool, query: &str, kinds: &[HitKind]) -> AppResult<Resolved> {
    let q = query.trim();
    if let Some(id) = q.strip_prefix("t:") {
        return Ok(Resolved {
            tracks: catalog::get_track_row(db, id)
                .await?
                .map(Arc::new)
                .into_iter()
                .collect(),
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
            source: None,
            artist: None,
        }),
        Some(hit) => resolve_hit(db, hit).await,
    }
}

pub async fn resolve_hit(db: &SqlitePool, hit: &SearchHit) -> AppResult<Resolved> {
    match hit.kind {
        HitKind::Track => Ok(Resolved {
            tracks: catalog::get_track_row(db, &hit.id)
                .await?
                .map(Arc::new)
                .into_iter()
                .collect(),
            source: None,
            artist: None,
        }),
        HitKind::Album => resolve_album(db, &hit.id).await,
        HitKind::Artist => resolve_artist(db, &hit.id).await,
    }
}

async fn resolve_album(db: &SqlitePool, id: &str) -> AppResult<Resolved> {
    let tracks = search::album_tracks(db, id).await?;
    let source = tracks.first().and_then(|t| t.album.clone());
    Ok(Resolved {
        tracks: tracks.into_iter().map(Arc::new).collect(),
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

async fn autocomplete_kinds(
    ctx: Context<'_>,
    partial: &str,
    kinds: &[HitKind],
) -> Vec<AutocompleteChoice> {
    if partial.trim().is_empty() {
        return Vec::new();
    }
    let hits = match search::search(&ctx.data().state.db, partial, kinds, 25).await {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(error = %e, "autocomplete search failed");
            return Vec::new();
        }
    };
    hits.into_iter()
        .map(|h| {
            let (prefix, label) = match h.kind {
                HitKind::Track => ("t", format!("{} · {}", h.title, h.subtitle)),
                HitKind::Album => ("al", format!("💿 {} · {}", h.title, h.subtitle)),
                HitKind::Artist => ("ar", format!("👤 {} · {}", h.title, h.subtitle)),
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
        Err(r) => return send::respond(ctx, r.view(&super::icons(ctx))).await,
    };
    let resolved = resolve(&identity.state.db, query, kinds).await?;
    if resolved.tracks.is_empty() {
        let what = match kinds {
            [HitKind::Album] => "No album",
            [HitKind::Artist] => "No artist",
            _ => "Nothing",
        };
        return send::respond(
            ctx,
            views::notice(
                &super::icons(ctx),
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
                views::error(&super::icons(ctx), "Couldn't join", &format!("-# {e}")),
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
    let cover = match art {
        Some(art) => Some(art),
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
            resolved.source.as_deref(),
            cover.as_ref(),
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
