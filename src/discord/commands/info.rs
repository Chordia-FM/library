//! `/bots` and `/lyrics`.

use super::{Context, Error};
use crate::discord::identity::Status;
use crate::discord::lyrics;
use crate::discord::ui::{send, views};

/// Lyrics for the track that is playing, from the file's own tags
#[poise::command(slash_command, guild_only)]
pub async fn lyrics(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = super::guild_of(ctx)?;
    let identity = ctx.data();
    let Some(player) = identity.player_arc(guild) else {
        return send::respond(
            ctx,
            views::notice(
                &super::icons(ctx),
                "Nothing playing",
                "-# Lyrics follow the current track.",
            ),
        )
        .await;
    };
    let snap = player.snapshot().await;
    let Some(cur) = &snap.current else {
        return send::respond(
            ctx,
            views::notice(
                &super::icons(ctx),
                "Nothing playing",
                "-# Lyrics follow the current track.",
            ),
        )
        .await;
    };
    let track = &cur.item.track;
    let raw = crate::catalog::get_track_lyrics(&identity.state.db, &track.id)
        .await?
        .unwrap_or_default();
    let pages = lyrics::pages(&lyrics::lines(&raw), lyrics::PAGE_CHARS);
    send::respond(
        ctx,
        views::lyrics(&snap, &track.title, &track.artist, &pages, 0),
    )
    .await
}

/// See every bot identity and which are free
#[poise::command(slash_command, guild_only)]
pub async fn bots(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = super::guild_of(ctx)?;
    let identities = match crate::discord::runtime() {
        Some(rt) => rt.identities.clone(),
        None => vec![ctx.data().clone()],
    };
    let mut lines = Vec::with_capacity(identities.len());
    for id in identities {
        let (playing_in, listeners) = match id.player_arc(guild) {
            Some(p) => {
                let snap = p.snapshot().await;
                (snap.voice_channel, snap.listeners)
            }
            None => (None, 0),
        };
        lines.push(views::BotLine {
            name: id.display_name_sync(),
            online: id.status() == Status::Online,
            playing_in,
            listeners,
        });
    }
    send::respond(ctx, views::bots(&super::icons(ctx), &lines)).await
}
