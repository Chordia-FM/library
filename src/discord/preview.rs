//! What the dashboard's layout editor shows: a view rendered by the same code that renders it for
//! Discord, as the JSON the bot would send, with the pictures as data URLs so the page can draw
//! them. One renderer, so the preview cannot drift from the message.
//!
//! The scene is real: the track the bot played last (else one from the library that has cover
//! art), its cover, its album as the queue, its artist's picture from the Hub, the library owner
//! as the listener who asked, and the server's own play log as the history. Only a library with
//! nothing in it falls back to a made-up evening. The pick is kept for a while so the preview does
//! not change under an owner while they type.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use chordia_contracts::discord_layout::{LayoutView, ViewLayout};
use chordia_contracts::user::EqConfig;
use serde::Serialize;
use serenity::all::{ChannelId, GuildId, UserId};
use sqlx::AssertSqlSafe;

use crate::catalog::{self, TrackRow, TRACK_COLS_NO_LIB, TRACK_JOINS};
use crate::discord::emoji::DEFAULT_HEX;
use crate::discord::hub;
use crate::discord::identity::Identity;
use crate::discord::lyrics;
use crate::discord::player::{
    Cover, CurrentSnapshot, Enqueued, LeaveReason, LoopMode, PlayerSnapshot, QueueItem, VoteTally,
};
use crate::discord::settings::{self, PlayEntry};
use crate::discord::source::TrackFacts;
use crate::discord::ui::{fmt, views};
use crate::http::AppState;
use crate::search::HitKind;

#[derive(Debug, Serialize)]
pub struct Preview {
    /// The message as the bot would send it (Components V2).
    pub body: serde_json::Value,
    /// The files it would upload, by filename, as data URLs.
    pub images: HashMap<String, String>,
    /// What the mentions in the message refer to, by id, so the page can draw them by name.
    pub mentions: Mentions,
}

#[derive(Debug, Default, Serialize)]
pub struct Mentions {
    pub channels: HashMap<String, String>,
    pub users: HashMap<String, String>,
    pub roles: HashMap<String, String>,
}

/// How long one pick of a sample track stays, per bot.
const PICK_TTL: Duration = Duration::from_secs(15 * 60);

static PICKS: LazyLock<Mutex<HashMap<String, (Instant, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// What the sample scene is built from.
struct Sample {
    /// The current track first, then what is queued after it.
    tracks: Vec<Arc<TrackRow>>,
    cover: Option<Cover>,
    artist_art: Option<Cover>,
    artist_banner: Option<Cover>,
}

/// The made-up evening, for a library with nothing in it (or nothing with a picture).
fn stand_in(hex: &str) -> anyhow::Result<Sample> {
    const DISCOVERY: [(&str, i64); 14] = [
        ("One More Time", 320_000),
        ("Aerodynamic", 212_000),
        ("Digital Love", 301_000),
        ("Harder, Better, Faster, Stronger", 224_000),
        ("Crescendolls", 211_000),
        ("Nightvision", 104_000),
        ("Superheroes", 237_000),
        ("High Life", 201_000),
        ("Something About Us", 232_000),
        ("Voyager", 227_000),
        ("Veridis Quo", 345_000),
        ("Short Circuit", 206_000),
        ("Face to Face", 240_000),
        ("Too Long", 600_000),
    ];
    let mark = Arc::new(crate::discord::avatar::render_png(hex)?);
    let tracks = DISCOVERY
        .iter()
        .enumerate()
        .map(|(i, (title, ms))| {
            Arc::new(TrackRow {
                id: format!("preview-{i}"),
                library_id: String::new(),
                content_hash: format!("preview-{i}"),
                title: (*title).into(),
                artist: "Daft Punk".into(),
                album_artist: Some("Daft Punk".into()),
                album: Some("Discovery".into()),
                year: Some(2001),
                genre: Some("House".into()),
                track_no: Some(i as i64 + 1),
                disc_no: Some(1),
                duration_ms: *ms,
                acoustid: None,
                recording_mbid: None,
                artist_norm: "daft punk".into(),
                title_norm: title.to_lowercase(),
                album_norm: Some("discovery".into()),
                codec: "flac".into(),
                sample_rate_hz: 44100,
                bit_depth: 16,
                channels: 2,
                lossless: 1,
                spatial: 0,
                rg_gain_db: Some(-7.1),
                rg_peak: Some(0.99),
            })
        })
        .collect();
    Ok(Sample {
        tracks,
        cover: Some(Cover {
            filename: "cover-preview.png".into(),
            bytes: mark.clone(),
            attachment_id: None,
        }),
        artist_art: Some(Cover {
            filename: "artist-preview.png".into(),
            bytes: mark.clone(),
            attachment_id: None,
        }),
        artist_banner: Some(Cover {
            filename: "banner-preview.png".into(),
            bytes: mark,
            attachment_id: None,
        }),
    })
}

/// The track to build the scene around: what the bot played last, else a random one with a cover,
/// remembered for a while.
async fn pick_track(state: &AppState, app_id: &str) -> Option<TrackRow> {
    let db = &state.db;
    let last: Option<String> = sqlx::query_scalar(
        "SELECT track_id FROM discord_plays WHERE app_id = ? ORDER BY started_at DESC LIMIT 1",
    )
    .bind(app_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if let Some(id) = last {
        if let Ok(Some(t)) = catalog::get_track_row(db, &id).await {
            return Some(t);
        }
    }
    let remembered = PICKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(app_id)
        .filter(|(at, _)| at.elapsed() < PICK_TTL)
        .map(|(_, id)| id.clone());
    if let Some(id) = remembered {
        if let Ok(Some(t)) = catalog::get_track_row(db, &id).await {
            return Some(t);
        }
    }
    let sql = format!(
        "SELECT {TRACK_COLS_NO_LIB} FROM tracks t {TRACK_JOINS} \
         WHERE COALESCE(t.cover_hash, al.cover_hash) IS NOT NULL \
         ORDER BY RANDOM() LIMIT 1"
    );
    let picked = sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;
    PICKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(app_id.to_string(), (Instant::now(), picked.id.clone()));
    Some(picked)
}

/// The rest of the evening: the track's album in order, padded from the library when it is short.
async fn queue_after(state: &AppState, track: &TrackRow) -> Vec<Arc<TrackRow>> {
    let db = &state.db;
    let album_id: Option<String> = sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
        .bind(&track.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let mut out: Vec<Arc<TrackRow>> = Vec::new();
    if let Some(album_id) = album_id {
        if let Ok(rows) = crate::search::album_tracks(db, &album_id).await {
            out.extend(rows.into_iter().filter(|t| t.id != track.id).map(Arc::new));
        }
    }
    if out.len() < 12 {
        let sql = format!(
            "SELECT {TRACK_COLS_NO_LIB} FROM tracks t {TRACK_JOINS} \
             WHERE t.id != ? ORDER BY RANDOM() LIMIT ?"
        );
        if let Ok(rows) = sqlx::query_as::<_, TrackRow>(AssertSqlSafe(sql))
            .bind(&track.id)
            .bind((12 - out.len()) as i64)
            .fetch_all(db)
            .await
        {
            for t in rows {
                if !out.iter().any(|x| x.id == t.id) {
                    out.push(Arc::new(t));
                }
            }
        }
    }
    out
}

async fn sample(identity: &Identity, hex: &str) -> anyhow::Result<Sample> {
    let state = &identity.state;
    let app_id = identity.app_id_sync().unwrap_or(0).to_string();
    let Some(track) = pick_track(state, &app_id).await else {
        return stand_in(hex);
    };
    let cover = Cover::load(&state.db, &track).await;
    let (artist_art, artist_banner) = match hub::artist_art(state, &track.artist_norm, None).await {
        Some(art) => hub::artist_pictures(state, &art).await,
        None => (None, None),
    };
    let mut tracks = vec![Arc::new(track.clone())];
    tracks.extend(queue_after(state, &track).await);
    Ok(Sample {
        tracks,
        cover,
        artist_art,
        artist_banner,
    })
}

/// Two pages of lyrics for a file that has none, so the pager has something to turn.
fn stand_in_lyrics() -> Vec<String> {
    vec![
        "Verse one goes here, a line at a time,\nthe way the tags in the file keep it.\n\nA chorus follows when there is one,\nand a page turns when it runs long.".to_string(),
        "The second page picks up the song;\nthe first and last buttons jump the ends.\n\n-# Real lyrics come from the file's own tags.".to_string(),
    ]
}

/// Render `view` with `layout` in place of the bot's saved one, over the sample scene.
pub async fn render(
    identity: &Identity,
    view: LayoutView,
    layout: &ViewLayout,
    guild: Option<GuildId>,
) -> anyhow::Result<Preview> {
    let settings = identity.settings();
    let mut layouts = settings.layouts.clone();
    *layouts.view_mut(view) = layout.clone();
    let hex = settings.emoji_hex.as_deref().unwrap_or(DEFAULT_HEX);
    let sample = sample(identity, hex).await?;

    // A real voice channel of the guild when there is one, so `{channel}` is a mention the page
    // can name; otherwise a made-up one.
    let (channel_id, channel_name) = guild
        .and_then(|g| {
            identity
                .guild_channels(g)
                .into_iter()
                .find(|c| c.kind == "voice")
        })
        .and_then(|c| c.id.parse::<u64>().ok().map(|id| (id, c.name)))
        .unwrap_or((1, "music".to_string()));
    // The library owner asked for everything, when the Hub knows their Discord account.
    let (listener, listener_name) = crate::discord::runtime()
        .and_then(|rt| rt.owner())
        .and_then(|o| o.discord_id?.parse::<u64>().ok().map(|id| (id, o.handle)))
        .unwrap_or((1, "listener".to_string()));
    let mut mentions = Mentions::default();
    mentions
        .channels
        .insert(channel_id.to_string(), channel_name.clone());
    mentions.users.insert(listener.to_string(), listener_name);
    if let Some(id) = identity.user_id() {
        mentions
            .users
            .insert(id.get().to_string(), identity.display_name_sync());
    }

    let item = |t: &Arc<TrackRow>| QueueItem {
        track: t.clone(),
        requested_by: UserId::new(listener),
        autoplay: false,
    };
    let current = &sample.tracks[0];
    let mut facts = TrackFacts::from_row(current);
    facts.opus_kbps = Some(96);
    let queue: Vec<QueueItem> = sample.tracks[1..].iter().map(item).collect();
    let snap = PlayerSnapshot {
        bot_index: identity.index,
        bot_name: identity.display_name_sync(),
        bot_avatar: identity.profile().and_then(|p| p.avatar_url),
        bot_user_id: identity.user_id().map(|u| u.get()),
        guild_name: guild.and_then(|g| identity.guild_name(g)),
        guild_icon: guild.and_then(|g| identity.guild_icon_url(g)),
        icons: identity.icons(),
        web_base: identity.web_base().await,
        guild_id: guild.unwrap_or(GuildId::new(1)),
        voice_channel: Some(ChannelId::new(channel_id)),
        voice_channel_name: Some(channel_name),
        current: Some(CurrentSnapshot {
            item: item(current),
            facts,
            position_ms: (current.duration_ms.max(0) as u64) / 5,
            paused: false,
            cover: sample.cover.clone(),
            artist_art: sample.artist_art.clone(),
            artist_banner: sample.artist_banner.clone(),
            links: hub::resolve_track(&identity.state, current).await,
        }),
        queue: queue.clone(),
        history: Vec::new(),
        loop_mode: LoopMode::Queue,
        autoplay: true,
        shuffle: false,
        volume: 80,
        muted: false,
        normalize: true,
        listeners: 3,
        eq: EqConfig::default(),
        layouts: Arc::new(layouts),
    };
    let message = match view {
        LayoutView::NowPlaying => views::now_playing(&snap, false),
        LayoutView::Idle => views::idle(&snap),
        LayoutView::Left => views::left(&snap, LeaveReason::Idle),
        LayoutView::Queued => views::queued(
            &snap,
            &queue[..queue.len().min(1)],
            &Enqueued {
                position: 3,
                count: 1,
            },
            views::Added {
                kind: HitKind::Track,
                source: None,
                url: None,
            },
            sample.cover.as_ref(),
            sample.artist_art.as_ref(),
            sample.artist_banner.as_ref(),
        ),
        // The whole sample as an album (its tracks are the current one's album, padded), and
        // again as the artist's tracks, with their picture for the cover as `/play` would.
        LayoutView::QueuedAlbum => views::queued(
            &snap,
            &queue,
            &Enqueued {
                position: 3,
                count: queue.len(),
            },
            views::Added {
                kind: HitKind::Album,
                source: current.album.as_deref(),
                url: None,
            },
            sample.cover.as_ref(),
            sample.artist_art.as_ref(),
            sample.artist_banner.as_ref(),
        ),
        LayoutView::QueuedArtist => views::queued(
            &snap,
            &queue,
            &Enqueued {
                position: 3,
                count: queue.len(),
            },
            views::Added {
                kind: HitKind::Artist,
                source: Some(&current.artist),
                url: None,
            },
            sample.artist_art.as_ref().or(sample.cover.as_ref()),
            sample.artist_art.as_ref(),
            sample.artist_banner.as_ref(),
        ),
        LayoutView::QueuedPlaylist => views::queued(
            &snap,
            &queue,
            &Enqueued {
                position: 3,
                count: queue.len(),
            },
            views::Added {
                kind: HitKind::Playlist,
                source: Some("Evening drive"),
                url: Some("/app/playlists"),
            },
            sample.cover.as_ref(),
            sample.artist_art.as_ref(),
            sample.artist_banner.as_ref(),
        ),
        LayoutView::Lyrics => {
            // The track's own lyrics when the file has them; else two pages of stand-in.
            let raw = lyrics::text_for(&identity.state, current)
                .await
                .unwrap_or_default();
            let mut pages = lyrics::pages(&lyrics::lines(&raw), lyrics::PAGE_CHARS);
            if pages.is_empty() {
                pages = stand_in_lyrics();
            }
            views::lyrics(&snap, current, &pages, 0)
        }
        LayoutView::Done => views::ok(
            &snap,
            "Skipped",
            &format!(
                "**{}** · {}",
                fmt::escape_md(&current.title),
                fmt::escape_md(&current.artist)
            ),
        ),
        LayoutView::Notice => {
            views::notice(&snap, "Nothing is playing", "-# `/play` something first.")
        }
        LayoutView::Error => views::error(
            &snap,
            "Couldn't join",
            "-# I'm not allowed to connect to that channel.",
        ),
        LayoutView::Vote => views::vote(
            &snap,
            &VoteTally {
                count: 2,
                needed: 3,
                listeners: 5,
                percent: 50,
                passed: false,
            },
            UserId::new(listener),
        ),
        LayoutView::VotePassed => views::vote(
            &snap,
            &VoteTally {
                count: 3,
                needed: 3,
                listeners: 5,
                percent: 50,
                passed: true,
            },
            UserId::new(listener),
        ),
        LayoutView::Queue => views::queue_page(&snap, 0),
        LayoutView::History => {
            // The server's own log when it has one; else the evening, as if it had been heard.
            let app_id = identity.app_id_sync().unwrap_or(0).to_string();
            let logged = match guild {
                Some(g) => settings::recent_plays(
                    &identity.state.db,
                    &app_id,
                    &g.get().to_string(),
                    views::HISTORY_LIMIT,
                )
                .await
                .unwrap_or_default(),
                None => Vec::new(),
            };
            let plays = if logged.is_empty() {
                let now = settings::now_ms();
                sample
                    .tracks
                    .iter()
                    .take(12)
                    .enumerate()
                    .map(|(i, t)| PlayEntry {
                        title: t.title.clone(),
                        artist: t.artist.clone(),
                        requested_by: (i % 3 != 2).then(|| listener.to_string()),
                        started_at: now - (i as i64 + 1) * 300_000,
                        ms_played: if i % 4 == 3 { 0 } else { t.duration_ms },
                        scrobbled_for: if i % 2 == 0 { 2 } else { 0 },
                    })
                    .collect()
            } else {
                logged
            };
            views::history(&snap, &plays, 0)
        }
    };
    message
        .validate()
        .map_err(|e| anyhow::anyhow!("that layout makes a message Discord would refuse: {e}"))?;
    let images = message
        .attachments
        .iter()
        .map(|a| {
            let mime = match a.filename.rsplit('.').next() {
                Some("png") => "image/png",
                Some("webp") => "image/webp",
                Some("gif") => "image/gif",
                _ => "image/jpeg",
            };
            (
                a.filename.clone(),
                format!(
                    "data:{mime};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&a.data)
                ),
            )
        })
        .collect();
    Ok(Preview {
        body: message.body(),
        images,
        mentions,
    })
}
