//! `/settings`, `/dj`, `/247`: what a server admin sets for this bot in this guild.

use serenity::all::Role;

use super::{guard, Context, Error};
use crate::discord::player::LeaveReason;
use crate::discord::ui::{send, views};

/// This server's settings for the bot
#[poise::command(slash_command, guild_only)]
pub async fn settings(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    let snap = player.snapshot().await;
    let gs = player.settings().await;
    send::respond(ctx, views::settings(&snap, &gs)).await
}

/// Set (or clear) the DJ role that may control shared playback
#[poise::command(slash_command, guild_only)]
pub async fn dj(
    ctx: Context<'_>,
    #[description = "The DJ role; leave empty to clear it"] role: Option<Role>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    let role_id = role.as_ref().map(|r| r.id.get().to_string());
    player
        .update_settings(|s| s.dj_role_id = role_id.clone())
        .await;
    let detail = match role {
        Some(r) => format!("-# <@&{}> controls shared playback now.", r.id.get()),
        None => "-# No DJ role: anyone in the channel can control playback.".to_string(),
    };
    send::respond(ctx, views::ok(&super::icons(ctx), "DJ role", &detail)).await
}

/// Keep the bot in its voice channel around the clock
#[poise::command(slash_command, guild_only, rename = "247")]
pub async fn always_on(
    ctx: Context<'_>,
    #[description = "On or off"] on: bool,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::icons(ctx))).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    if on {
        // The channel to keep: the one the bot is in, else the caller's.
        let channel = match player.voice_channel().await {
            Some(c) => c,
            None => match guard::listener(ctx).await {
                Ok((vc, _)) => {
                    if let Err(e) = player.join(vc, ctx.channel_id()).await {
                        return send::respond(
                            ctx,
                            views::error(&super::icons(ctx), "Couldn't join", &format!("-# {e}")),
                        )
                        .await;
                    }
                    vc
                }
                Err(r) => return send::respond(ctx, r.view(&super::icons(ctx))).await,
            },
        };
        player
            .update_settings(|s| {
                s.always_on = true;
                s.always_on_channel_id = Some(channel.get().to_string());
            })
            .await;
        send::respond(
            ctx,
            views::ok(
                &super::icons(ctx),
                "24/7 on",
                &format!(
                    "-# I'll stay in <#{}> and come back after restarts.",
                    channel.get()
                ),
            ),
        )
        .await
    } else {
        player
            .update_settings(|s| {
                s.always_on = false;
                s.always_on_channel_id = None;
            })
            .await;
        // Nothing playing and nobody listening: leave now rather than at the next idle check.
        if !player.is_playing().await && !player.has_listeners().await {
            player.leave(LeaveReason::Idle).await;
        }
        send::respond(ctx, views::ok(&super::icons(ctx), "24/7 off", "")).await
    }
}
