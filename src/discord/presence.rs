//! The bot's presence ("Listening to …") and the voice channel's status line.
//!
//! Both are cheap to set and rate-limited to abuse, so both go through a last-value check: a
//! change is sent once, a repeat is dropped. Presence is per bot user and global across guilds:
//! in single-server mode it follows the one guild's track through the owner's template, in
//! multi-server mode it rotates through the owner's list of statuses. The voice-channel status
//! is per channel and follows the track; single-server mode can switch it off.

use std::time::{Duration, Instant};

use serde_json::json;
use serenity::all::{ActivityData, ChannelId};

use crate::catalog::TrackRow;
use crate::discord::identity::Identity;
use crate::discord::settings::{BotMode, BotSettings};
use crate::discord::ui::fmt;

/// Discord caps an activity name at 128 characters.
const ACTIVITY_MAX: usize = 128;
/// Discord caps a voice channel status at 500 characters.
const VC_STATUS_MAX: usize = 500;
/// How long a library track count is reused for `{tracks}`.
const TRACK_COUNT_TTL: Duration = Duration::from_secs(600);

/// Recompute and (if changed) send the presence for an identity. Called on every player change
/// and on every ticker beat, so a rotating status advances on time.
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
    let text = match settings.mode {
        BotMode::Single => single_text(identity, &settings).await,
        BotMode::Multi => multi_text(identity, &settings).await,
    };
    let text = text.unwrap_or_else(|| "/play".to_string());
    fmt::ellipsize(text.trim(), ACTIVITY_MAX)
}

/// The one active guild's track through the template; nothing while nothing plays.
async fn single_text(identity: &Identity, settings: &BotSettings) -> Option<String> {
    // The one active guild — or the first one playing, if the bot was invited to several.
    for player in identity.players() {
        let snap = player.snapshot().await;
        let Some(cur) = &snap.current else { continue };
        let guild = identity.guild_name(snap.guild_id).unwrap_or_default();
        let channel = snap
            .voice_channel
            .and_then(|c| identity.channel_name(snap.guild_id, c))
            .unwrap_or_default();
        let listeners = snap.listeners.to_string();
        let t = &cur.item.track;
        let album = t.album.clone().unwrap_or_default();
        return Some(fmt::render_template(
            &settings.presence_template,
            &[
                ("title", t.title.as_str()),
                ("artist", t.artist.as_str()),
                ("album", album.as_str()),
                ("guild", guild.as_str()),
                ("channel", channel.as_str()),
                ("listeners", listeners.as_str()),
            ],
        ));
    }
    None
}

/// The status whose turn it is, with the fleet-wide numbers filled in.
async fn multi_text(identity: &Identity, settings: &BotSettings) -> Option<String> {
    let list = &settings.multi_statuses;
    if list.is_empty() {
        return None;
    }
    let slot = identity.status_slot(list.len(), settings.status_rotate_secs);
    let template = list.get(slot)?;
    let servers = identity.guild_ids().len().to_string();
    let (mut playing, mut listeners) = (0usize, 0usize);
    if template.contains("{playing}") || template.contains("{listeners}") {
        for player in identity.players() {
            if player.is_playing().await {
                playing += 1;
            }
            listeners += player.listener_count().await;
        }
    }
    let tracks = if template.contains("{tracks}") {
        track_count(identity).await.to_string()
    } else {
        String::new()
    };
    let bot = identity.display_name_sync();
    Some(fmt::render_template(
        template,
        &[
            ("servers", servers.as_str()),
            ("playing", playing.to_string().as_str()),
            ("listeners", listeners.to_string().as_str()),
            ("tracks", tracks.as_str()),
            ("bot", bot.as_str()),
        ],
    ))
}

/// The library's track count, refreshed every few minutes rather than per status change.
async fn track_count(identity: &Identity) -> u64 {
    {
        let cached = identity
            .track_count
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((at, n)) = *cached {
            if at.elapsed() < TRACK_COUNT_TTL {
                return n;
            }
        }
    }
    let n = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tracks")
        .fetch_one(&identity.state.db)
        .await
        .unwrap_or(0)
        .max(0) as u64;
    *identity
        .track_count
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), n));
    n
}

/// Whether the voice channel's status should follow the track: always in multi-server mode,
/// by the owner's choice in single-server mode.
fn voice_status_on(settings: &BotSettings) -> bool {
    settings.mode == BotMode::Multi || settings.vc_status
}

/// Set the voice channel's status line to the playing track.
pub async fn set_voice_status(identity: &Identity, channel: ChannelId, track: Option<&TrackRow>) {
    if !voice_status_on(&identity.settings()) {
        return;
    }
    let status = track
        .map(|t| {
            fmt::ellipsize(
                &format!("{} {} · {}", fmt::glyph::NOTE, t.title, t.artist),
                VC_STATUS_MAX,
            )
        })
        .unwrap_or_default();
    send_voice_status(identity, channel, status).await;
}

pub async fn clear_voice_status(identity: &Identity, channel: ChannelId) {
    if !voice_status_on(&identity.settings()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_status_follows_mode() {
        let mut s = BotSettings::defaults("1");
        s.vc_status = false;
        assert!(
            voice_status_on(&s),
            "multi-server mode always shows the track"
        );
        s.mode = BotMode::Single;
        assert!(!voice_status_on(&s));
        s.vc_status = true;
        assert!(voice_status_on(&s));
    }
}
