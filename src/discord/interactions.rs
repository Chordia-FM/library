//! Button presses and select picks, dispatched by [`CustomId`].
//!
//! Discord gives an interaction three seconds before it shows "This interaction failed", and the
//! work behind a press (stopping a track, decoding the next one, uploading a cover) can take
//! longer than that. So every press is acknowledged **first** and acted on second:
//!
//! - Controller buttons answer with a bare "update coming" acknowledgement; the controller task
//!   redraws the message a moment later, so a burst of presses costs one edit. A refusal or an
//!   error goes out as an ephemeral follow-up.
//! - Views that belong to one person (queue pages, pickers) are deferred the same way and then
//!   replaced through the interaction's edit endpoint, which stays valid for fifteen minutes.
//! - The Queue button opens a new private message: a deferred ephemeral response, then the edit.

use std::sync::Arc;

use serenity::all::{ComponentInteraction, ComponentInteractionDataKind, Context, GuildId};

use crate::discord::commands::guard;
use crate::discord::commands::play::resolve;
use crate::discord::identity::Identity;
use crate::discord::player::{Cover, GuildPlayer, PlayerError, Position, QueueItem};
use crate::discord::ui::custom_id::{Action, CustomId};
use crate::discord::ui::{send, views};
use crate::search::HitKind;

pub async fn handle(
    identity: &Arc<Identity>,
    ctx: &Context,
    ic: &ComponentInteraction,
) -> anyhow::Result<()> {
    let http = &ctx.http;
    let Ok(cid) = ic.data.custom_id.parse::<CustomId>() else {
        // Not ours (or a stale schema). Acknowledge so the user's client stops spinning.
        return send::component_ack(http, ic).await;
    };
    let guild = GuildId::new(cid.guild);
    if cid.bot != identity.index || ic.guild_id != Some(guild) {
        return send::component_ack(http, ic).await;
    }
    let icons = identity.icons();
    let user = ic.user.id;
    let token = ic.token.as_str();

    // Acknowledge before anything that can take time.
    match cid.action {
        Action::QueueOpen => send::component_defer_ephemeral(http, ic).await?,
        _ => send::component_ack(http, ic).await?,
    }
    let player = identity.player(guild).await;

    match cid.action {
        Action::PlayPause
        | Action::Skip
        | Action::Previous
        | Action::Stop
        | Action::Shuffle
        | Action::LoopCycle
        | Action::VolumeUp
        | Action::VolumeDown
        | Action::AutoplayToggle => {
            if let Err(r) =
                guard::controller_for(identity, &player, guild, user, ic.member.as_ref()).await
            {
                return send::interaction_followup(http, token, r.view(&icons)).await;
            }
            if let Err(e) = controlled_action(&cid.action, &player).await {
                let err = views::error(&icons, "Couldn't do that", &format!("-# {e}"));
                send::interaction_followup(http, token, err).await?;
            }
            Ok(())
        }
        Action::QueueOpen | Action::QueueFirst => {
            let snap = player.snapshot().await;
            send::interaction_edit(http, token, views::queue_page(&snap, 0)).await
        }
        Action::Queue(page) => {
            let snap = player.snapshot().await;
            send::interaction_edit(http, token, views::queue_page(&snap, page as usize)).await
        }
        Action::QueueLast => {
            let snap = player.snapshot().await;
            // The view clamps to the last page.
            send::interaction_edit(http, token, views::queue_page(&snap, usize::MAX)).await
        }
        Action::Refresh => {
            let snap = player.snapshot().await;
            send::interaction_edit(http, token, views::now_playing(&snap, false).ephemeral()).await
        }
        Action::Cancel => send::interaction_edit(http, token, views::cancelled()).await,
        Action::Select(_) | Action::Play(_) => {
            let value = match (&cid.action, &ic.data.kind) {
                (Action::Play(id), _) => format!("t:{id}"),
                (_, ComponentInteractionDataKind::StringSelect { values }) => {
                    match values.first() {
                        Some(v) => v.clone(),
                        None => return Ok(()),
                    }
                }
                _ => return Ok(()),
            };
            let (vc, player) = match guard::listener_for(identity, guild, user).await {
                Ok(x) => x,
                Err(r) => return send::interaction_edit(http, token, r.view(&icons)).await,
            };
            let resolved = resolve(
                &identity.state.db,
                &value,
                &[HitKind::Track, HitKind::Album, HitKind::Artist],
            )
            .await?;
            if resolved.tracks.is_empty() {
                return send::interaction_edit(
                    http,
                    token,
                    views::notice(&icons, "Gone", "-# That track is no longer in the library."),
                )
                .await;
            }
            if player.voice_channel().await != Some(vc) {
                if let Err(e) = player.join(vc, ic.channel_id).await {
                    return send::interaction_edit(
                        http,
                        token,
                        views::error(&icons, "Couldn't join", &format!("-# {e}")),
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
                    send::interaction_edit(http, token, toast).await
                }
                Err(e) => {
                    send::interaction_edit(
                        http,
                        token,
                        views::error(&icons, "Couldn't queue that", &format!("-# {e}")),
                    )
                    .await
                }
            }
        }
        // Later phases: confirmations and settings toggles.
        Action::Lyrics | Action::Confirm(_) | Action::Setting(_) => Ok(()),
    }
}

async fn controlled_action(action: &Action, player: &Arc<GuildPlayer>) -> Result<(), PlayerError> {
    match action {
        Action::PlayPause => player.toggle_pause().await.map(|_| ()),
        Action::Skip => player.skip().await.map(|_| ()),
        Action::Previous => player.previous().await.map(|_| ()),
        Action::Stop => player.stop().await,
        Action::Shuffle => {
            player.shuffle().await;
            Ok(())
        }
        Action::LoopCycle => {
            player.cycle_loop().await;
            Ok(())
        }
        Action::VolumeUp => {
            let v = player.volume().await;
            player
                .set_volume(v.saturating_add(10).min(150))
                .await
                .map(|_| ())
        }
        Action::VolumeDown => {
            let v = player.volume().await;
            player.set_volume(v.saturating_sub(10)).await.map(|_| ())
        }
        Action::AutoplayToggle => {
            let snap = player.snapshot().await;
            player.set_autoplay(!snap.autoplay).await;
            Ok(())
        }
        _ => Ok(()),
    }
}
