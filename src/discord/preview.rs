//! What the dashboard's layout editor shows: a view rendered from a sample scene by the same code
//! that renders it for Discord, as the JSON the bot would send, with the pictures as data URLs so
//! the page can draw them. One renderer, so the preview cannot drift from the message.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use chordia_contracts::discord_layout::{LayoutView, ViewLayout};
use serde::Serialize;
use serenity::all::{GuildId, UserId};

use crate::catalog::TrackRow;
use crate::discord::emoji::DEFAULT_HEX;
use crate::discord::identity::Identity;
use crate::discord::player::{
    Cover, CurrentSnapshot, Enqueued, LeaveReason, LoopMode, PlayerSnapshot, QueueItem,
};
use crate::discord::source::TrackFacts;
use crate::discord::ui::views;

#[derive(Debug, Serialize)]
pub struct Preview {
    /// The message as the bot would send it (Components V2).
    pub body: serde_json::Value,
    /// The files it would upload, by filename, as data URLs.
    pub images: HashMap<String, String>,
}

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

/// Render `view` with `layout` in place of the bot's saved one, over a sample scene.
pub async fn render(
    identity: &Identity,
    view: LayoutView,
    layout: &ViewLayout,
) -> anyhow::Result<Preview> {
    let settings = identity.settings();
    let mut layouts = settings.layouts.clone();
    match view {
        LayoutView::NowPlaying => layouts.now_playing = layout.clone(),
        LayoutView::Idle => layouts.idle = layout.clone(),
        LayoutView::Queued => layouts.queued = layout.clone(),
        LayoutView::Left => layouts.left = layout.clone(),
    }
    // The sample cover is the mark in the bot's colour: always available, and clearly a stand-in.
    let hex = settings.emoji_hex.as_deref().unwrap_or(DEFAULT_HEX);
    let cover = Cover {
        filename: "cover-preview.png".into(),
        bytes: Arc::new(crate::discord::avatar::render_png(hex)?),
        attachment_id: None,
    };
    let current_track = sample_track("preview-1", "One More Time", "Discovery", 320_000);
    let item = |t: &Arc<TrackRow>| QueueItem {
        track: t.clone(),
        requested_by: UserId::new(1),
        autoplay: false,
    };
    let mut facts = TrackFacts::from_row(&current_track);
    facts.opus_kbps = Some(96);
    let queue = vec![
        item(&sample_track(
            "preview-2",
            "Aerodynamic",
            "Discovery",
            212_000,
        )),
        item(&sample_track(
            "preview-3",
            "Digital Love",
            "Discovery",
            301_000,
        )),
    ];
    let snap = PlayerSnapshot {
        bot_index: identity.index,
        bot_name: identity.display_name_sync(),
        icons: identity.icons(),
        web_base: identity.web_base().await,
        guild_id: GuildId::new(1),
        voice_channel: None,
        voice_channel_name: Some("music".into()),
        current: Some(CurrentSnapshot {
            item: item(&current_track),
            facts,
            position_ms: 65_000,
            paused: false,
            cover: Some(cover.clone()),
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
        ),
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
    })
}
