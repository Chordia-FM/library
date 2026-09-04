//! The bot's presence ("Listening to …") and the voice channel's status line.
//!
//! Both are cheap to set and rate-limited to abuse, so both go through a last-value check: a
//! change is sent once, a repeat is dropped. Presence is per bot user and global across guilds,
//! which is why it can only follow a track in single-server mode; the voice-channel status is per
//! channel and follows the track everywhere.

use serde_json::json;
use serenity::all::{ActivityData, ChannelId};

use crate::catalog::TrackRow;
use crate::discord::identity::Identity;
use crate::discord::settings::BotMode;
use crate::discord::ui::fmt;

/// Discord caps an activity name at 128 characters.
const ACTIVITY_MAX: usize = 128;
/// Discord caps a voice channel status at 500 characters.
const VC_STATUS_MAX: usize = 500;

/// Recompute and (if changed) send the presence for an identity.
pub async fn update(identity: &Identity) {
    let Some(ctx) = identity.context() else {
        return;
    };
    let text = activity_text(identity).await;
    {
        let mut last = identity
            .last_activity
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last.as_deref() == Some(text.as_str()) {
            return;
        }
        *last = Some(text.clone());
    }
    ctx.set_activity(Some(ActivityData::listening(text)));
}

/// What "Listening to …" should say right now.
async fn activity_text(identity: &Identity) -> String {
    let settings = identity.settings();
    if settings.mode == BotMode::Single {
        // The one active guild — or the first one playing, if the bot was invited to several.
        for player in identity.players() {
            let snap = player.snapshot().await;
            if let Some(cur) = &snap.current {
                let guild = identity.guild_name(snap.guild_id).unwrap_or_default();
                let channel = snap
                    .voice_channel
                    .and_then(|c| identity.channel_name(snap.guild_id, c))
                    .unwrap_or_default();
                let listeners = snap.listeners.to_string();
                let t = &cur.item.track;
                let album = t.album.clone().unwrap_or_default();
                let rendered = fmt::render_template(
                    &settings.presence_template,
                    &[
                        ("title", t.title.as_str()),
                        ("artist", t.artist.as_str()),
                        ("album", album.as_str()),
                        ("guild", guild.as_str()),
                        ("channel", channel.as_str()),
                        ("listeners", listeners.as_str()),
                    ],
                );
                return fmt::ellipsize(rendered.trim(), ACTIVITY_MAX);
            }
        }
    }
    "/play".to_string()
}

/// Set the voice channel's status line to the playing track (if the identity has that on).
pub async fn set_voice_status(identity: &Identity, channel: ChannelId, track: Option<&TrackRow>) {
    if !identity.settings().vc_status {
        return;
    }
    let status = track
        .map(|t| {
            fmt::ellipsize(
                &format!("{} {} — {}", fmt::glyph::NOTE, t.title, t.artist),
                VC_STATUS_MAX,
            )
        })
        .unwrap_or_default();
    send_voice_status(identity, channel, status).await;
}

pub async fn clear_voice_status(identity: &Identity, channel: ChannelId) {
    if !identity.settings().vc_status {
        return;
    }
    send_voice_status(identity, channel, String::new()).await;
}

async fn send_voice_status(identity: &Identity, channel: ChannelId, status: String) {
    let Some(http) = identity.http() else { return };
    {
        let mut last = identity
            .last_vc_status
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last.get(&channel).map(String::as_str) == Some(status.as_str()) {
            return;
        }
        last.insert(channel, status.clone());
    }
    let body = if status.is_empty() {
        json!({ "status": null })
    } else {
        json!({ "status": status })
    };
    if let Err(e) = http.edit_voice_status(channel, &body, None).await {
        // Missing SET_VOICE_CHANNEL_STATUS is the usual cause; say so once, quietly.
        tracing::debug!(channel = %channel, error = %e, "setting voice channel status");
    }
}
