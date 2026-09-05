//! `/queue`, `/nowplaying`, `/history`, and the queue edits.

use super::{guard, Context, Error};
use crate::discord::ui::{fmt, send, views};

async fn player_or_notice(
    ctx: Context<'_>,
) -> Result<Option<std::sync::Arc<crate::discord::player::GuildPlayer>>, Error> {
    let identity = ctx.data();
    let guild = super::guild_of(ctx)?;
    match identity.player_arc(guild) {
        Some(p) => Ok(Some(p)),
        None => {
            send::respond(
                ctx,
                views::notice(
                    &super::icons(ctx),
                    "Nothing here yet",
                    "-# `/play` something first.",
                ),
            )
            .await?;
            Ok(None)
        }
    }
}

/// Show the queue
#[poise::command(slash_command, guild_only)]
pub async fn queue(
    ctx: Context<'_>,
    #[description = "Page number"]
    #[min = 1]
    page: Option<u32>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    let snap = player.snapshot().await;
    let page = page.unwrap_or(1).saturating_sub(1) as usize;
    send::respond(ctx, views::queue_page(&snap, page)).await
}

/// What's playing right now
#[poise::command(slash_command, guild_only, aliases("np"))]
pub async fn nowplaying(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    let snap = player.snapshot().await;
    send::respond(ctx, views::now_playing(&snap, false).ephemeral()).await
}

/// Recently played tracks
#[poise::command(slash_command, guild_only)]
pub async fn history(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    let identity = ctx.data();
    let guild = super::guild_of(ctx)?;
    let icons = super::icons(ctx);
    let Some(app_id) = identity.app_id_sync() else {
        return send::respond(ctx, guard::Refusal::Offline.view(&icons)).await;
    };
    let plays = crate::discord::settings::recent_plays(
        &identity.state.db,
        &app_id.to_string(),
        &guild.get().to_string(),
        views::HISTORY_LIMIT,
    )
    .await?;
    let snap = player.snapshot().await;
    send::respond(ctx, views::history(&snap, &plays, 0)).await
}

/// Remove a track, or a run of tracks, from the queue
#[poise::command(slash_command, guild_only)]
pub async fn remove(
    ctx: Context<'_>,
    #[description = "Its number in /queue"]
    #[min = 1]
    index: u32,
    #[description = "The last number of a range to remove, for more than one"]
    #[min = 1]
    to: Option<u32>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    if let Err(r) = guard::controller(ctx, &player).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    let to = to.unwrap_or(index).max(index) as usize;
    match player.remove_range(index as usize, to).await {
        Ok(items) if items.len() == 1 => {
            let item = &items[0];
            send::respond(
                ctx,
                views::ok(
                    &super::icons(ctx),
                    "Removed",
                    &format!(
                        "**{}** · {}",
                        fmt::escape_md(&item.track.title),
                        fmt::escape_md(&item.track.artist)
                    ),
                ),
            )
            .await
        }
        Ok(items) => {
            let (first, last) = (&items[0].track, &items[items.len() - 1].track);
            send::respond(
                ctx,
                views::ok(
                    &super::icons(ctx),
                    &format!("Removed {}", fmt::count(items.len(), "track")),
                    &format!(
                        "-# #{index} **{}** through #{} **{}**",
                        fmt::escape_md(&first.title),
                        index as usize + items.len() - 1,
                        fmt::escape_md(&last.title)
                    ),
                ),
            )
            .await
        }
        Err(e) => {
            send::respond(
                ctx,
                views::error(
                    &super::icons(ctx),
                    "Couldn't remove that",
                    &format!("-# {e}"),
                ),
            )
            .await
        }
    }
}

/// Move a track to another position in the queue
#[poise::command(slash_command, guild_only, rename = "move")]
pub async fn move_track(
    ctx: Context<'_>,
    #[description = "Its number in /queue"]
    #[min = 1]
    from: u32,
    #[description = "Where it should go"]
    #[min = 1]
    to: u32,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    if let Err(r) = guard::controller(ctx, &player).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    match player.move_item(from as usize, to as usize).await {
        Ok(item) => {
            send::respond(
                ctx,
                views::ok(
                    &super::icons(ctx),
                    "Moved",
                    &format!("**{}** is now #{to}", fmt::escape_md(&item.track.title)),
                ),
            )
            .await
        }
        Err(e) => {
            send::respond(
                ctx,
                views::error(&super::icons(ctx), "Couldn't move that", &format!("-# {e}")),
            )
            .await
        }
    }
}

/// Jump to a track in the queue, skipping everything before it
#[poise::command(slash_command, guild_only)]
pub async fn jump(
    ctx: Context<'_>,
    #[description = "Its number in /queue"]
    #[min = 1]
    index: u32,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    if let Err(r) = guard::controller(ctx, &player).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    match player.jump(index as usize).await {
        Ok(item) => {
            send::respond(
                ctx,
                views::ok(
                    &super::icons(ctx),
                    "Jumping to",
                    &format!(
                        "**{}** · {}",
                        fmt::escape_md(&item.track.title),
                        fmt::escape_md(&item.track.artist)
                    ),
                ),
            )
            .await
        }
        Err(e) => {
            send::respond(
                ctx,
                views::error(
                    &super::icons(ctx),
                    "Couldn't jump there",
                    &format!("-# {e}"),
                ),
            )
            .await
        }
    }
}

/// Clear the queue (keeps the current track playing)
#[poise::command(slash_command, guild_only)]
pub async fn clear(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    if let Err(r) = guard::controller(ctx, &player).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    let n = player.clear().await;
    send::respond(
        ctx,
        views::ok(
            &super::icons(ctx),
            "Queue cleared",
            &format!("-# {} dropped", fmt::count(n, "track")),
        ),
    )
    .await
}

/// Play the queue in random order, or back in order
#[poise::command(slash_command, guild_only)]
pub async fn shuffle(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let Some(player) = player_or_notice(ctx).await? else {
        return Ok(());
    };
    if let Err(r) = guard::controller(ctx, &player).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    let on = player.toggle_shuffle().await;
    send::respond(
        ctx,
        views::ok(
            &super::icons(ctx),
            "Shuffle",
            if on {
                "-# on: the queue plays in random order."
            } else {
                "-# off: the queue plays in order."
            },
        ),
    )
    .await
}
