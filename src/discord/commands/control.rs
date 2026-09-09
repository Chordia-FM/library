//! Transport: `/skip`, `/back`, `/pause`, `/resume`, `/stop`, `/seek`, `/volume`, `/loop`,
//! `/join`, `/leave`, `/radio`.

use std::sync::Arc;
use std::time::Duration;

use super::{guard, Context, Error};
use crate::discord::player::{GuildPlayer, LeaveReason, LoopMode, PlayerError};
use crate::discord::ui::{fmt, send, views};

/// Fetch the guild's player and run the controller guard; answers the refusal itself.
async fn controlled(ctx: Context<'_>) -> Result<Option<Arc<GuildPlayer>>, Error> {
    let identity = ctx.data();
    let guild = super::guild_of(ctx)?;
    let Some(player) = identity.player_arc(guild) else {
        send::respond(
            ctx,
            views::notice(
                &super::snap(ctx).await,
                "Nothing is playing",
                "-# `/play` something first.",
            ),
        )
        .await?;
        return Ok(None);
    };
    if let Err(r) = guard::controller(ctx, &player).await {
        send::respond(ctx, r.view(&super::snap(ctx).await)).await?;
        return Ok(None);
    }
    Ok(Some(player))
}

fn track_line(item: &crate::discord::player::QueueItem) -> String {
    format!(
        "**{}** · {}",
        fmt::escape_md(&item.track.title),
        fmt::escape_md(&item.track.artist)
    )
}

async fn player_error(ctx: Context<'_>, title: &str, e: PlayerError) -> Result<(), Error> {
    send::respond(
        ctx,
        views::error(&super::snap(ctx).await, title, &format!("-# {e}")),
    )
    .await
}

/// Skip the current track
#[poise::command(slash_command, guild_only)]
pub async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    match player.skip().await {
        Ok(item) => {
            send::respond(
                ctx,
                views::ok(&super::snap(ctx).await, "Skipped", &track_line(&item)),
            )
            .await
        }
        Err(e) => player_error(ctx, "Couldn't skip", e).await,
    }
}

/// Go back to the previous track
#[poise::command(slash_command, guild_only)]
pub async fn back(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    match player.previous().await {
        Ok(item) => {
            send::respond(
                ctx,
                views::ok(&super::snap(ctx).await, "Going back to", &track_line(&item)),
            )
            .await
        }
        Err(e) => player_error(ctx, "Couldn't go back", e).await,
    }
}

/// Pause playback
#[poise::command(slash_command, guild_only)]
pub async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    match player.pause().await {
        Ok(()) => send::respond(ctx, views::ok(&super::snap(ctx).await, "Paused", "")).await,
        Err(e) => player_error(ctx, "Couldn't pause", e).await,
    }
}

/// Resume playback
#[poise::command(slash_command, guild_only)]
pub async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    match player.resume().await {
        Ok(()) => send::respond(ctx, views::ok(&super::snap(ctx).await, "Resumed", "")).await,
        Err(e) => player_error(ctx, "Couldn't resume", e).await,
    }
}

/// Stop playing and clear the queue
#[poise::command(slash_command, guild_only)]
pub async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    match player.stop().await {
        Ok(()) => {
            send::respond(
                ctx,
                views::ok(
                    &super::snap(ctx).await,
                    "Stopped",
                    "-# Queue cleared. I'll stay in the channel for a bit.",
                ),
            )
            .await
        }
        Err(e) => player_error(ctx, "Couldn't stop", e).await,
    }
}

/// Parse `1:23`, `1:02:03`, `90`, `+30`, `-15` into an absolute position.
pub fn parse_seek(input: &str, current_ms: u64) -> Option<u64> {
    let s = input.trim();
    let (sign, rest) = match s.chars().next()? {
        '+' => (1i8, &s[1..]),
        '-' => (-1i8, &s[1..]),
        _ => (0i8, s),
    };
    let mut total: u64 = 0;
    for part in rest.split(':') {
        let n: u64 = part.trim().parse().ok()?;
        total = total.checked_mul(60)?.checked_add(n)?;
    }
    let ms = total.checked_mul(1000)?;
    Some(match sign {
        1 => current_ms.saturating_add(ms),
        -1 => current_ms.saturating_sub(ms),
        _ => ms,
    })
}

/// Seek within the current track
#[poise::command(slash_command, guild_only)]
pub async fn seek(
    ctx: Context<'_>,
    #[description = "A time like 1:23, or +30 / -30 to move relative to now"] position: String,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    let current = player.position().await.unwrap_or_default().as_millis() as u64;
    let Some(target) = parse_seek(&position, current) else {
        return send::respond(
            ctx,
            views::error(
                &super::snap(ctx).await,
                "That's not a time",
                "-# Try `1:23`, `90`, `+30` or `-30`.",
            ),
        )
        .await;
    };
    match player.seek(Duration::from_millis(target)).await {
        Ok(got) => {
            send::respond(
                ctx,
                views::ok(
                    &super::snap(ctx).await,
                    "Seeked",
                    &format!("-# now at {}", fmt::duration(got.as_millis() as u64)),
                ),
            )
            .await
        }
        Err(e) => player_error(ctx, "Couldn't seek", e).await,
    }
}

/// Set the volume (100 is normal, 150 the ceiling)
#[poise::command(slash_command, guild_only)]
pub async fn volume(
    ctx: Context<'_>,
    #[description = "0–150"]
    #[min = 0]
    #[max = 150]
    level: u32,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    match player.set_volume(level.min(150) as u8).await {
        Ok(v) => {
            send::respond(
                ctx,
                views::ok(
                    &super::snap(ctx).await,
                    "Volume",
                    &format!("{} {v}%", fmt::glyph::VOLUME),
                ),
            )
            .await
        }
        Err(e) => player_error(ctx, "Couldn't set the volume", e).await,
    }
}

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum LoopChoice {
    #[name = "off"]
    Off,
    #[name = "track"]
    Track,
    #[name = "queue"]
    Queue,
}

/// Repeat the track or the whole queue
#[poise::command(slash_command, guild_only, rename = "loop")]
pub async fn loop_mode(
    ctx: Context<'_>,
    #[description = "What to repeat"] mode: LoopChoice,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    let mode = match mode {
        LoopChoice::Off => LoopMode::Off,
        LoopChoice::Track => LoopMode::Track,
        LoopChoice::Queue => LoopMode::Queue,
    };
    let set = player.set_loop(mode).await;
    send::respond(
        ctx,
        views::ok(
            &super::snap(ctx).await,
            "Loop",
            &format!("-# {}", set.label()),
        ),
    )
    .await
}

/// Bring the bot into your voice channel
#[poise::command(slash_command, guild_only)]
pub async fn join(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let (vc, player) = match guard::listener(ctx).await {
        Ok(x) => x,
        Err(r) => return send::respond(ctx, r.view(&super::snap(ctx).await)).await,
    };
    match player.join(vc, ctx.channel_id()).await {
        Ok(()) => {
            send::respond(
                ctx,
                views::ok(
                    &super::snap(ctx).await,
                    "Joined",
                    &format!("-# <#{}>", vc.get()),
                ),
            )
            .await
        }
        Err(e) => player_error(ctx, "Couldn't join", e).await,
    }
}

/// Stop and leave the voice channel
#[poise::command(slash_command, guild_only)]
pub async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    player.leave(LeaveReason::Command).await;
    send::respond(ctx, views::ok(&super::snap(ctx).await, "Left", "")).await
}

/// Keep the music going with similar tracks when the queue runs out
#[poise::command(slash_command, guild_only)]
pub async fn radio(ctx: Context<'_>, #[description = "On or off"] on: bool) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = controlled(ctx).await? else {
        return Ok(());
    };
    if on && !player.settings().await.can_autoplay {
        let r = super::guard::Refusal::NotAllowed("autoplay");
        return send::respond(ctx, r.view(&super::snap(ctx).await)).await;
    }
    let on = player.set_autoplay(on).await;
    send::respond(
        ctx,
        views::ok(
            &super::snap(ctx).await,
            "Autoplay",
            &format!(
                "-# {}",
                if on {
                    "on. Similar tracks follow the queue"
                } else {
                    "off"
                }
            ),
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::parse_seek;

    #[test]
    fn seek_formats() {
        assert_eq!(parse_seek("1:23", 0), Some(83_000));
        assert_eq!(parse_seek("1:02:03", 0), Some(3_723_000));
        assert_eq!(parse_seek("90", 0), Some(90_000));
        assert_eq!(parse_seek("+30", 10_000), Some(40_000));
        assert_eq!(parse_seek("-30", 10_000), Some(0));
        assert_eq!(parse_seek("abc", 0), None);
        assert_eq!(parse_seek("", 0), None);
        assert_eq!(parse_seek("1:", 0), None);
    }
}
