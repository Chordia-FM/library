//! What the dashboard's layout editor shows: a view rendered from a sample scene by the same code
//! that renders it for Discord, as the JSON the bot would send, with the pictures as data URLs so
//! the page can draw them. One renderer, so the preview cannot drift from the message.
//!
//! The scene is the same every time (a Daft Punk evening) so an owner comparing two layouts sees
//! the layout change and nothing else. With a guild, the channel is one of that guild's, so a
//! `{channel}` mention resolves to a real name, and the page gets the names it needs to draw the
//! mentions the message contains.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use chordia_contracts::discord_layout::{LayoutView, ViewLayout};
use serde::Serialize;
use serenity::all::{ChannelId, GuildId, UserId};

use crate::catalog::TrackRow;
use crate::discord::emoji::DEFAULT_HEX;
use crate::discord::identity::Identity;
use crate::discord::player::{
    Cover, CurrentSnapshot, Enqueued, LeaveReason, LoopMode, PlayerSnapshot, QueueItem,
};
use crate::discord::settings::PlayEntry;
use crate::discord::source::TrackFacts;
use crate::discord::ui::views;

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

/// The listener who asked for everything in the sample scene.
const LISTENER: u64 = 1;

fn sample_track(id: &str, title: &str, album: &str, duration_ms: i64) -> Arc<TrackRow> {
    Arc::new(TrackRow {
        id: id.into(),
        library_id: String::new(),
        content_hash: format!("preview-{id}"),
        title: title.into(),
        artist: "Daft Punk".into(),
        album_artist: Some("Daft Punk".into()),
        album: Some(album.into()),
        year: Some(2001),
        genre: Some("House".into()),
        track_no: None,
        disc_no: None,
        duration_ms,
        acoustid: None,
        recording_mbid: None,
        artist_norm: "daft punk".into(),
        title_norm: title.to_lowercase(),
        album_norm: Some(album.to_lowercase()),
        codec: "flac".into(),
        sample_rate_hz: 44100,
        bit_depth: 16,
        channels: 2,
        lossless: 1,
        spatial: 0,
        rg_gain_db: Some(-7.1),
        rg_peak: Some(0.99),
    })
}

/// The album, in order, for a queue long enough to page.
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

/// Render `view` with `layout` in place of the bot's saved one, over a sample scene.
pub async fn render(
    identity: &Identity,
    view: LayoutView,
    layout: &ViewLayout,
    guild: Option<GuildId>,
) -> anyhow::Result<Preview> {
    let settings = identity.settings();
    let mut layouts = settings.layouts.clone();
    *layouts.view_mut(view) = layout.clone();
    // The sample pictures are the mark in the bot's colour: always available, and clearly a
    // stand-in. The artist's is the same picture under another name, so a layout that shows both
    // shows two.
    let hex = settings.emoji_hex.as_deref().unwrap_or(DEFAULT_HEX);
    let mark = Arc::new(crate::discord::avatar::render_png(hex)?);
    let cover = Cover {
        filename: "cover-preview.png".into(),
        bytes: mark.clone(),
        attachment_id: None,
    };
    let artist_art = Cover {
        filename: "artist-preview.png".into(),
        bytes: mark,
        attachment_id: None,
    };
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
    let mut mentions = Mentions::default();
    mentions
        .channels
        .insert(channel_id.to_string(), channel_name.clone());
    mentions
        .users
        .insert(LISTENER.to_string(), "listener".into());

    let tracks: Vec<Arc<TrackRow>> = DISCOVERY
        .iter()
        .enumerate()
        .map(|(i, (title, ms))| sample_track(&format!("preview-{i}"), title, "Discovery", *ms))
        .collect();
    let item = |t: &Arc<TrackRow>| QueueItem {
        track: t.clone(),
        requested_by: UserId::new(LISTENER),
        autoplay: false,
    };
    let mut facts = TrackFacts::from_row(&tracks[0]);
    facts.opus_kbps = Some(96);
    let queue: Vec<QueueItem> = tracks[1..].iter().map(item).collect();
    let snap = PlayerSnapshot {
        bot_index: identity.index,
        bot_name: identity.display_name_sync(),
        bot_avatar: identity.profile().and_then(|p| p.avatar_url),
        icons: identity.icons(),
        web_base: identity.web_base().await,
        guild_id: guild.unwrap_or(GuildId::new(1)),
        voice_channel: Some(ChannelId::new(channel_id)),
        voice_channel_name: Some(channel_name),
        current: Some(CurrentSnapshot {
            item: item(&tracks[0]),
            facts,
            position_ms: 65_000,
            paused: false,
            cover: Some(cover.clone()),
            artist_art: Some(artist_art.clone()),
            links: None,
        }),
        queue: queue.clone(),
        history: Vec::new(),
        loop_mode: LoopMode::Queue,
        autoplay: true,
        shuffle: false,
        volume: 80,
        normalize: true,
        listeners: 3,
        layouts: Arc::new(layouts),
    };
    let message = match view {
        LayoutView::NowPlaying => views::now_playing(&snap, false),
        LayoutView::Idle => views::idle(&snap),
        LayoutView::Left => views::left(&snap, LeaveReason::Idle),
        LayoutView::Queued => views::queued(
            &snap,
            &queue[..1],
            &Enqueued {
                position: 3,
                count: 1,
            },
            None,
            Some(&cover),
            Some(&artist_art),
        ),
        LayoutView::Queue => views::queue_page(&snap, 0),
        LayoutView::History => {
            let now = crate::discord::settings::now_ms();
            let plays: Vec<PlayEntry> = tracks
                .iter()
                .take(12)
                .enumerate()
                .map(|(i, t)| PlayEntry {
                    title: t.title.clone(),
                    artist: t.artist.clone(),
                    requested_by: (i % 3 != 2).then(|| LISTENER.to_string()),
                    started_at: now - (i as i64 + 1) * 300_000,
                    ms_played: if i % 4 == 3 { 0 } else { t.duration_ms },
                    scrobbled_for: if i % 2 == 0 { 2 } else { 0 },
                })
                .collect();
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
