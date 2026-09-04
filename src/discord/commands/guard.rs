//! Who may do what.
//!
//! Two questions, asked before anything touches the player:
//!
//! - **Listener**: is the caller in a voice channel, and is this bot free to serve it? A bot already
//!   playing to people in another channel of the same guild refuses and names a sibling that is
//!   free. A bot alone in a channel, or idle, simply moves.
//! - **Controller**: may the caller change what everyone hears (skip, stop, seek, volume, loop,
//!   queue edits)? Yes if any holds: they are a configured owner of the bot; they can manage the
//!   server; the guild has no DJ role; they hold the DJ role; or they are the only listener.
//!   Otherwise the answer names the DJ role.
//!
//! Both are plain functions over ids and members so the slash commands and the button presses
//! share them.

use std::sync::Arc;

use serenity::all::{ChannelId, GuildId, Member, Permissions, RoleId, UserId};

use super::Context;
use crate::discord::identity::Identity;
use crate::discord::player::GuildPlayer;
use crate::discord::ui::{views, Message};

#[derive(Debug)]
pub enum Refusal {
    NotInVoice,
    Busy {
        bot_name: String,
        channel_name: String,
        listeners: usize,
        free: Vec<String>,
    },
    NeedDj {
        role: Option<RoleId>,
    },
    Offline,
}

impl Refusal {
    pub fn view(&self) -> Message {
        match self {
            Refusal::NotInVoice => views::notice(
                "Join a voice channel first",
                "-# I play where you are — hop into a voice channel and try again.",
            ),
            Refusal::Busy {
                bot_name,
                channel_name,
                listeners,
                free,
            } => views::busy(bot_name, channel_name, *listeners, free),
            Refusal::NeedDj { role } => {
                let who = match role {
                    Some(r) => format!("<@&{}>", r.get()),
                    None => "a DJ".to_string(),
                };
                views::notice(
                    "That's a DJ control",
                    &format!(
                        "Only {who} (or someone who manages the server) can change what everyone hears while others are listening.\n-# Alone in the channel? Then it's all yours."
                    ),
                )
            }
            Refusal::Offline => views::error(
                "Not connected",
                "-# This bot is reconnecting to Discord. Try again in a moment.",
            ),
        }
    }
}

/// The caller must be in a voice channel this bot can serve. Returns their channel and the player.
pub async fn listener(ctx: Context<'_>) -> Result<(ChannelId, Arc<GuildPlayer>), Refusal> {
    let identity = ctx.data();
    let guild = ctx.guild_id().ok_or(Refusal::NotInVoice)?;
    listener_for(identity, guild, ctx.author().id).await
}

pub async fn listener_for(
    identity: &Arc<Identity>,
    guild: GuildId,
    user: UserId,
) -> Result<(ChannelId, Arc<GuildPlayer>), Refusal> {
    if identity.http().is_none() {
        return Err(Refusal::Offline);
    }
    let vc = identity
        .member_voice_channel(guild, user)
        .ok_or(Refusal::NotInVoice)?;
    let player = identity.player(guild).await;
    if let Some(current) = player.voice_channel().await {
        if current != vc && player.has_listeners().await {
            let free = match crate::discord::runtime() {
                Some(rt) => rt
                    .free_siblings(guild, identity.index)
                    .await
                    .into_iter()
                    .map(|i| i.display_name_sync())
                    .collect(),
                None => Vec::new(),
            };
            let snap = player.snapshot().await;
            return Err(Refusal::Busy {
                bot_name: identity.display_name_sync(),
                channel_name: identity
                    .channel_name(guild, current)
                    .unwrap_or_else(|| "voice".into()),
                listeners: snap.listeners,
                free,
            });
        }
    }
    Ok((vc, player))
}

/// The caller must be allowed to change shared playback.
pub async fn controller(ctx: Context<'_>, player: &GuildPlayer) -> Result<(), Refusal> {
    let identity = ctx.data();
    let guild = ctx.guild_id().ok_or(Refusal::NotInVoice)?;
    let member = ctx.author_member().await;
    controller_for(identity, player, guild, ctx.author().id, member.as_deref()).await
}

pub async fn controller_for(
    identity: &Identity,
    player: &GuildPlayer,
    _guild: GuildId,
    user: UserId,
    member: Option<&Member>,
) -> Result<(), Refusal> {
    if identity.settings().is_owner(user.get()) {
        return Ok(());
    }
    if let Some(m) = member {
        if m.permissions.is_some_and(|p| {
            p.contains(Permissions::MANAGE_GUILD) || p.contains(Permissions::ADMINISTRATOR)
        }) {
            return Ok(());
        }
    }
    let dj_role: Option<RoleId> = player
        .settings()
        .await
        .dj_role_id
        .and_then(|r| r.parse::<u64>().ok())
        .map(RoleId::new);
    let Some(role) = dj_role else {
        return Ok(());
    };
    if member.is_some_and(|m| m.roles.contains(&role)) {
        return Ok(());
    }
    if player.is_alone_with(user).await {
        return Ok(());
    }
    Err(Refusal::NeedDj { role: Some(role) })
}
