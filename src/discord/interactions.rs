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
        Action::QueueOpen | Action::Lyrics => send::component_defer_ephemeral(http, ic).await?,
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
            if matches!(cid.action, Action::AutoplayToggle)
                && !player.snapshot().await.autoplay
                && !player.settings().await.can_autoplay
            {
                let r = guard::Refusal::NotAllowed("autoplay");
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
        Action::Select(ref ctx_name) if ctx_name == "dj" => {
            if let Err(r) = guard::admin_for(identity, user, ic.member.as_ref()) {
                return send::interaction_followup(http, token, r.view(&icons)).await;
            }
            let roles: Vec<String> = match &ic.data.kind {
                ComponentInteractionDataKind::RoleSelect { values } => {
                    values.iter().map(|r| r.get().to_string()).collect()
                }
                _ => Vec::new(),
            };
            player
                .update_settings(|s| s.dj_role_ids = roles.clone())
                .await;
            let snap = player.snapshot().await;
            let gs = player.settings().await;
            send::interaction_edit(http, token, views::settings(&snap, &gs)).await
        }
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
            let art_url = crate::discord::commands::play::art_for(&identity.state, &resolved).await;
            let items: Vec<QueueItem> = resolved
                .tracks
                .into_iter()
                .map(|track| QueueItem {
                    track,
                    requested_by: user,
                    autoplay: false,
                })
                .collect();
            let cover = match art_url {
                Some(_) => None,
                None => Cover::load(&identity.state.db, &items[0].track).await,
            };
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
                        art_url.as_deref(),
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
        Action::Lyrics | Action::LyricsPage(_) => {
            let page = match cid.action {
                Action::LyricsPage(p) => p as usize,
                _ => 0,
            };
            let snap = player.snapshot().await;
            let Some(cur) = &snap.current else {
                return send::interaction_edit(
                    http,
                    token,
                    views::notice(
                        &icons,
                        "Nothing playing",
                        "-# Lyrics follow the current track.",
                    ),
                )
                .await;
            };
            let track = &cur.item.track;
            let raw = crate::catalog::get_track_lyrics(&identity.state.db, &track.id)
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            let pages = crate::discord::lyrics::pages(
                &crate::discord::lyrics::lines(&raw),
                crate::discord::lyrics::PAGE_CHARS,
            );
            send::interaction_edit(
                http,
                token,
                views::lyrics(&snap, &track.title, &track.artist, &pages, page),
            )
            .await
        }
        Action::Setting(name) => {
            if let Err(r) = guard::admin_for(identity, user, ic.member.as_ref()) {
                return send::interaction_followup(http, token, r.view(&icons)).await;
            }
            let voice = player.voice_channel().await;
            let gs = player.settings().await;
            let blocked = match name.as_str() {
                "always_on" if !gs.can_always_on => Some("24/7"),
                "autoplay" if !gs.can_autoplay => Some("autoplay"),
                _ => None,
            };
            if let Some(what) = blocked {
                let r = guard::Refusal::NotAllowed(what);
                return send::interaction_followup(http, token, r.view(&icons)).await;
            }
            player
                .update_settings(|s| match name.as_str() {
                    "normalize" => s.normalize = !s.normalize,
                    "autoplay" => s.autoplay = !s.autoplay,
                    "announce" => s.announce = !s.announce,
                    "dj_clear" => s.dj_role_ids.clear(),
                    "always_on" => {
                        s.always_on = !s.always_on;
                        s.always_on_channel_id = if s.always_on {
                            voice.map(|c| c.get().to_string())
                        } else {
                            None
                        };
                    }
                    _ => {}
                })
                .await;
            let snap = player.snapshot().await;
            let gs = player.settings().await;
            send::interaction_edit(http, token, views::settings(&snap, &gs)).await
        }
        // Confirmations arrive with the destructive queue actions.
        Action::Confirm(_) => Ok(()),
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
