//! `/settings`, `/dj`, `/247`, `/skipmode`: what a server admin sets for this bot in this guild.

use serenity::all::Role;

use super::{guard, Context, Error};
use crate::discord::player::LeaveReason;
use crate::discord::settings::{Pickup, SkipMode};
use crate::discord::ui::{send, views};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum SkipChoice {
    #[name = "single"]
    Single,
    #[name = "vote"]
    Vote,
}

/// How a track gets skipped: by one person, or by a vote among the listeners
#[poise::command(slash_command, guild_only)]
pub async fn skipmode(
    ctx: Context<'_>,
    #[description = "Single: whoever may control playback skips at once. Vote: listeners vote"]
    mode: SkipChoice,
    #[description = "Share of the listeners a vote needs, 1 to 100 (default 50)"]
    #[min = 1]
    #[max = 100]
    percent: Option<u8>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::snap(ctx).await)).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    let updated = player
        .update_settings(|s| {
            s.skip_mode = match mode {
                SkipChoice::Single => SkipMode::Single,
                SkipChoice::Vote => SkipMode::Vote,
            };
            if let Some(p) = percent {
                s.vote_percent = p.clamp(1, 100);
            }
        })
        .await;
    let detail = match updated.skip_mode {
        SkipMode::Single => "-# Whoever may control playback skips at once; `/forceskip` is for DJs.".to_string(),
        SkipMode::Vote => format!(
            "-# `/skip` or `/vote` casts a vote; the track goes at {}% of the listeners. DJs can still `/forceskip`.",
            updated.vote_percent
        ),
    };
    send::respond(ctx, views::ok(&super::snap(ctx).await, "Skipping", &detail)).await
}

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum PickupChoice {
    #[name = "position"]
    Position,
    #[name = "start"]
    Start,
}

/// Where the bot picks up after a restart: where it left off, or at the start of the track
#[poise::command(slash_command, guild_only)]
pub async fn pickup(
    ctx: Context<'_>,
    #[description = "Where it left off, or the start of the track"] mode: PickupChoice,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::snap(ctx).await)).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    let updated = player
        .update_settings(|s| {
            s.pickup = match mode {
                PickupChoice::Position => Pickup::Position,
                PickupChoice::Start => Pickup::Start,
            }
        })
        .await;
    let detail = match updated.pickup {
        Pickup::Position => {
            "-# When the library restarts mid-track, the bot comes back to the same spot."
        }
        Pickup::Start => "-# When the library restarts mid-track, the bot starts that track over.",
    };
    send::respond(
        ctx,
        views::ok(&super::snap(ctx).await, "After a restart", detail),
    )
    .await
}

/// This server's settings for the bot
#[poise::command(slash_command, guild_only)]
pub async fn settings(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::snap(ctx).await)).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    let snap = player.snapshot().await;
    let gs = player.settings().await;
    send::respond(ctx, views::settings(&snap, &gs)).await
}

/// Add or remove a DJ role (those roles may control shared playback)
#[poise::command(slash_command, guild_only)]
pub async fn dj(
    ctx: Context<'_>,
    #[description = "A role to add, or remove if it is already a DJ role; leave empty to clear all"]
    role: Option<Role>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::snap(ctx).await)).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    let detail = match role {
        Some(r) => {
            let id = r.id.get().to_string();
            let updated = player
                .update_settings(|s| match s.dj_role_ids.iter().position(|x| *x == id) {
                    Some(i) => {
                        s.dj_role_ids.remove(i);
                    }
                    None => s.dj_role_ids.push(id.clone()),
                })
                .await;
            let list = updated
                .dj_roles()
                .iter()
                .map(|r| format!("<@&{r}>"))
                .collect::<Vec<_>>()
                .join(", ");
            if updated.dj_role_ids.contains(&id) {
                format!("-# <@&{}> added. DJs: {list}", r.id.get())
            } else if list.is_empty() {
                format!(
                    "-# <@&{}> removed. No DJ roles left: anyone in the channel can control playback.",
                    r.id.get()
                )
            } else {
                format!("-# <@&{}> removed. DJs: {list}", r.id.get())
            }
        }
        None => {
            player.update_settings(|s| s.dj_role_ids.clear()).await;
            "-# No DJ roles: anyone in the channel can control playback.".to_string()
        }
    };
    send::respond(ctx, views::ok(&super::snap(ctx).await, "DJ roles", &detail)).await
}

/// Keep the bot in its voice channel around the clock
#[poise::command(slash_command, guild_only, rename = "247")]
pub async fn always_on(
    ctx: Context<'_>,
    #[description = "On or off"] on: bool,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    if let Err(r) = guard::admin(ctx).await {
        return send::respond(ctx, r.view(&super::snap(ctx).await)).await;
    }
    let guild = super::guild_of(ctx)?;
    let player = ctx.data().player(guild).await;
    if on && !player.settings().await.can_always_on {
        let r = guard::Refusal::NotAllowed("24/7");
        return send::respond(ctx, r.view(&super::snap(ctx).await)).await;
    }
    if on {
        // The channel to keep: the one the bot is in, else the caller's.
        let channel = match player.voice_channel().await {
            Some(c) => c,
            None => match guard::listener(ctx).await {
                Ok((vc, _)) => {
                    if let Err(e) = player.join(vc, ctx.channel_id()).await {
                        return send::respond(
                            ctx,
                            views::error(
                                &super::snap(ctx).await,
                                "Couldn't join",
                                &format!("-# {e}"),
                            ),
                        )
                        .await;
                    }
                    vc
                }
                Err(r) => return send::respond(ctx, r.view(&super::snap(ctx).await)).await,
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
                &super::snap(ctx).await,
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
        send::respond(ctx, views::ok(&super::snap(ctx).await, "24/7 off", "")).await
    }
}
