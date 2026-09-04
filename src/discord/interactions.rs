//! Button presses and select picks, dispatched by [`CustomId`].
//!
//! Controller buttons change the player and answer with a bare acknowledgement — the controller
//! task redraws the message itself a moment later, so a burst of presses costs one edit. Views that
//! belong to one person (queue pages, pickers) are replaced in place with `UPDATE_MESSAGE`.

use std::sync::Arc;

use serenity::all::{ComponentInteraction, ComponentInteractionDataKind, Context, GuildId};

use crate::discord::commands::guard;
use crate::discord::commands::play::resolve;
use crate::discord::identity::Identity;
use crate::discord::player::{Cover, Position, QueueItem};
use crate::discord::ui::custom_id::{Action, CustomId};
use crate::discord::ui::{send, views};
use crate::search::HitKind;

pub async fn handle(
    identity: &Arc<Identity>,
    ctx: &Context,
    ic: &ComponentInteraction,
) -> anyhow::Result<()> {
    let Ok(cid) = ic.data.custom_id.parse::<CustomId>() else {
        // Not ours (or a stale schema). Acknowledge so the user's client stops spinning.
        return send::component_ack(&ctx.http, ic).await;
    };
    let guild = GuildId::new(cid.guild);
    if cid.bot != identity.index || ic.guild_id != Some(guild) {
        return send::component_ack(&ctx.http, ic).await;
    }
    let player = identity.player(guild).await;
    let user = ic.user.id;
    let http = &ctx.http;

    macro_rules! controlled {
        ($body:expr) => {{
            if let Err(r) =
                guard::controller_for(identity, &player, guild, user, ic.member.as_ref()).await
            {
                return send::component_reply(http, ic, r.view(&identity.icons())).await;
            }
            let result: Result<(), crate::discord::player::PlayerError> = $body;
            match result {
                Ok(()) => send::component_ack(http, ic).await,
                Err(e) => {
                    send::component_reply(
                        http,
                        ic,
                        views::error(&identity.icons(), "Couldn't do that", &format!("-# {e}")),
                    )
                    .await
                }
            }
        }};
    }

    match cid.action {
        Action::PlayPause => controlled!(player.toggle_pause().await.map(|_| ())),
        Action::Skip => controlled!(player.skip().await.map(|_| ())),
        Action::Previous => controlled!(player.previous().await.map(|_| ())),
        Action::Stop => controlled!(player.stop().await),
        Action::Shuffle => controlled!({
            player.shuffle().await;
            Ok(())
        }),
        Action::LoopCycle => controlled!({
            player.cycle_loop().await;
            Ok(())
        }),
        Action::VolumeUp => controlled!({
            let v = player.volume().await;
            player
                .set_volume(v.saturating_add(10).min(150))
                .await
                .map(|_| ())
        }),
        Action::VolumeDown => controlled!({
            let v = player.volume().await;
            player.set_volume(v.saturating_sub(10)).await.map(|_| ())
        }),
        Action::AutoplayToggle => controlled!({
            let snap = player.snapshot().await;
            player.set_autoplay(!snap.autoplay).await;
            Ok(())
        }),
        Action::QueueOpen => {
            let snap = player.snapshot().await;
            send::component_reply(http, ic, views::queue_page(&snap, 0)).await
        }
        Action::Queue(page) => {
            let snap = player.snapshot().await;
            send::component_update(http, ic, views::queue_page(&snap, page as usize)).await
        }
        Action::Refresh => {
            let snap = player.snapshot().await;
            send::component_update(http, ic, views::now_playing(&snap, false).ephemeral()).await
        }
        Action::Cancel => send::component_update(http, ic, views::cancelled()).await,
        Action::Select(_) | Action::Play(_) => {
            let value = match (&cid.action, &ic.data.kind) {
                (Action::Play(id), _) => format!("t:{id}"),
                (_, ComponentInteractionDataKind::StringSelect { values }) => {
                    match values.first() {
                        Some(v) => v.clone(),
                        None => return send::component_ack(http, ic).await,
                    }
                }
                _ => return send::component_ack(http, ic).await,
            };
            let (vc, player) = match guard::listener_for(identity, guild, user).await {
                Ok(x) => x,
                Err(r) => return send::component_reply(http, ic, r.view(&identity.icons())).await,
            };
            let resolved = resolve(
                &identity.state.db,
                &value,
                &[HitKind::Track, HitKind::Album, HitKind::Artist],
            )
            .await?;
            if resolved.tracks.is_empty() {
                return send::component_update(
                    http,
                    ic,
                    views::notice(
                        &identity.icons(),
                        "Gone",
                        "-# That track is no longer in the library.",
                    ),
                )
                .await;
            }
            if player.voice_channel().await != Some(vc) {
                if let Err(e) = player.join(vc, ic.channel_id).await {
                    return send::component_update(
                        http,
                        ic,
                        views::error(&identity.icons(), "Couldn't join", &format!("-# {e}")),
                    )
                    .await;
                }
            }
            let items: Vec<QueueItem> = resolved
                .tracks
                .into_iter()
                .map(|track| QueueItem {
                    track,
                    requested_by: user,
                })
                .collect();
            let cover = Cover::load(&identity.state.db, &items[0].track).await;
            match player.enqueue(items.clone(), Position::Last).await {
                Ok(enq) => {
                    let snap = player.snapshot().await;
                    // The picker was ephemeral; the confirmation replaces it and stays private.
                    let toast = views::queued(
                        &snap,
                        &items,
                        &enq,
                        resolved.source.as_deref(),
                        cover.as_ref(),
                    )
                    .ephemeral();
                    send::component_update(http, ic, toast).await
                }
                Err(e) => {
                    send::component_update(
                        http,
                        ic,
                        views::error(&identity.icons(), "Couldn't queue that", &format!("-# {e}")),
                    )
                    .await
                }
            }
        }
        // Later phases: confirmations and settings toggles.
        Action::Lyrics | Action::Confirm(_) | Action::Setting(_) => {
            send::component_ack(http, ic).await
        }
    }
}
