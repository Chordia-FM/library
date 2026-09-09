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
use std::time::Duration;

use serenity::all::{ComponentInteraction, ComponentInteractionDataKind, Context, GuildId};

use crate::discord::commands::guard;
use crate::discord::commands::play::resolve;
use crate::discord::eq;
use crate::discord::identity::Identity;
use crate::discord::player::{
    Cover, GuildPlayer, LeaveReason, PlayerError, PlayerSnapshot, Position, QueueItem,
};
use crate::discord::settings::{self, PlayEntry, SkipMode};
use crate::discord::ui::custom_id::{Action, CustomId, Origin};
use crate::discord::ui::{send, views, Message};
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
    let user = ic.user.id;
    let token = ic.token.as_str();

    // Acknowledge before anything that can take time.
    match cid.action {
        Action::QueueOpen | Action::HistoryOpen | Action::Lyrics | Action::EqOpen => {
            send::component_defer_ephemeral(http, ic).await?
        }
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
        | Action::Mute
        | Action::SeekBack
        | Action::SeekForward
        | Action::Clear
        | Action::Leave
        | Action::AutoplayToggle => {
            // Under the vote rule a Skip press is a vote, from anyone listening.
            if matches!(cid.action, Action::Skip)
                && player.settings().await.skip_mode == SkipMode::Vote
            {
                let snap = player.snapshot().await;
                let msg = if player.is_listener(user).await {
                    match player.vote_skip(user).await {
                        Ok(tally) => views::vote(&snap, &tally, user),
                        Err(e) => views::error(&snap, "Couldn't vote", &format!("-# {e}")),
                    }
                } else {
                    views::notice(
                        &snap,
                        "Join the voice channel to vote",
                        "-# Only listeners vote.",
                    )
                };
                return send::interaction_followup(http, token, msg).await;
            }
            if let Err(r) =
                guard::controller_for(identity, &player, guild, user, ic.member.as_ref()).await
            {
                return send::interaction_followup(http, token, r.view(&player.snapshot().await))
                    .await;
            }
            if matches!(cid.action, Action::AutoplayToggle)
                && !player.snapshot().await.autoplay
                && !player.settings().await.can_autoplay
            {
                let r = guard::Refusal::NotAllowed("autoplay");
                return send::interaction_followup(http, token, r.view(&player.snapshot().await))
                    .await;
            }
            if let Err(e) = controlled_action(&cid.action, &player).await {
                let err = views::error(
                    &player.snapshot().await,
                    "Couldn't do that",
                    &format!("-# {e}"),
                );
                return send::interaction_followup(http, token, err).await;
            }
            // Pressed on a queue, history or lyrics page: what it shows has just changed.
            if let Some(origin) = cid.origin {
                let snap = player.snapshot().await;
                let msg = match origin {
                    Origin::Queue(p) => views::queue_page(&snap, p as usize),
                    Origin::History(p) => {
                        views::history(&snap, &recent(identity, guild).await?, p as usize)
                    }
                    Origin::Lyrics(p) => lyrics_page(identity, &snap, p as usize).await,
                };
                return send::interaction_edit(http, token, msg).await;
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
        Action::HistoryOpen | Action::History(_) | Action::HistoryFirst | Action::HistoryLast => {
            let page = match cid.action {
                Action::History(p) => p as usize,
                Action::HistoryLast => usize::MAX,
                _ => 0,
            };
            let plays = recent(identity, guild).await?;
            let snap = player.snapshot().await;
            send::interaction_edit(http, token, views::history(&snap, &plays, page)).await
        }
        Action::Refresh => {
            let snap = player.snapshot().await;
            send::interaction_edit(http, token, views::now_playing(&snap, false).ephemeral()).await
        }
        Action::EqOpen => send::interaction_edit(http, token, eq_panel(&player).await).await,
        Action::EqStep(_) | Action::EqFlat | Action::EqToggle => {
            if let Err(r) =
                guard::controller_for(identity, &player, guild, user, ic.member.as_ref()).await
            {
                return send::interaction_followup(http, token, r.view(&player.snapshot().await))
                    .await;
            }
            let band = player.eq_band().await;
            player
                .update_settings(|s| match cid.action {
                    Action::EqStep(n) => {
                        if let Some(b) = s.eq.bands.get_mut(band) {
                            b.gain = (b.gain + f32::from(n)).clamp(-eq::MAX_DB, eq::MAX_DB);
                        }
                        s.eq.enabled = true;
                    }
                    Action::EqFlat => {
                        s.eq = eq::preset_config(eq::preset("Flat").expect("flat"));
                    }
                    Action::EqToggle => s.eq.enabled = !s.eq.enabled,
                    _ => {}
                })
                .await;
            send::interaction_edit(http, token, eq_panel(&player).await).await
        }
        Action::Select(ref ctx_name) if ctx_name == "eq_preset" || ctx_name == "eq_band" => {
            if let Err(r) =
                guard::controller_for(identity, &player, guild, user, ic.member.as_ref()).await
            {
                return send::interaction_followup(http, token, r.view(&player.snapshot().await))
                    .await;
            }
            let value = match &ic.data.kind {
                ComponentInteractionDataKind::StringSelect { values } => {
                    values.first().cloned().unwrap_or_default()
                }
                _ => return Ok(()),
            };
            if let Some(name) = value.strip_prefix("p:") {
                if let Some(p) = eq::preset(name) {
                    player
                        .update_settings(|s| s.eq = eq::preset_config(p))
                        .await;
                }
            } else if let Some(i) = value.strip_prefix("b:").and_then(|i| i.parse().ok()) {
                player.set_eq_band(i).await;
            }
            send::interaction_edit(http, token, eq_panel(&player).await).await
        }
        Action::Cancel => send::interaction_edit(http, token, views::cancelled()).await,
        Action::Select(ref ctx_name) if ctx_name == "dj" => {
            if let Err(r) = guard::admin_for(identity, user, ic.member.as_ref()) {
                return send::interaction_followup(http, token, r.view(&player.snapshot().await))
                    .await;
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
                Err(r) => {
                    return send::interaction_edit(http, token, r.view(&player.snapshot().await))
                        .await
                }
            };
            let resolved = resolve(
                &identity.state,
                &value,
                &[HitKind::Track, HitKind::Album, HitKind::Artist],
                user,
            )
            .await?;
            if resolved.tracks.is_empty() {
                return send::interaction_edit(
                    http,
                    token,
                    views::notice(
                        &player.snapshot().await,
                        "Gone",
                        "-# That track is no longer in the library.",
                    ),
                )
                .await;
            }
            if player.voice_channel().await != Some(vc) {
                if let Err(e) = player.join(vc, ic.channel_id).await {
                    return send::interaction_edit(
                        http,
                        token,
                        views::error(
                            &player.snapshot().await,
                            "Couldn't join",
                            &format!("-# {e}"),
                        ),
                    )
                    .await;
                }
            }
            let art = crate::discord::commands::play::art_for(&identity.state, &resolved).await;
            let items: Vec<QueueItem> = resolved
                .tracks
                .into_iter()
                .map(|track| QueueItem {
                    track,
                    requested_by: user,
                    autoplay: false,
                })
                .collect();
            let cover = match &art.image {
                Some(a) => Some(a.clone()),
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
                        views::Added {
                            kind: resolved.kind,
                            source: resolved.source.as_deref(),
                            url: resolved.source_url.as_deref(),
                        },
                        cover.as_ref(),
                        art.image.as_ref(),
                        art.banner.as_ref(),
                    )
                    .ephemeral();
                    send::interaction_edit(http, token, toast).await
                }
                Err(e) => {
                    send::interaction_edit(
                        http,
                        token,
                        views::error(
                            &player.snapshot().await,
                            "Couldn't queue that",
                            &format!("-# {e}"),
                        ),
                    )
                    .await
                }
            }
        }
        Action::Lyrics | Action::LyricsPage(_) | Action::LyricsFirst | Action::LyricsLast => {
            let page = match cid.action {
                Action::LyricsPage(p) => p as usize,
                Action::LyricsLast => usize::MAX,
                _ => 0,
            };
            let snap = player.snapshot().await;
            let msg = lyrics_page(identity, &snap, page).await;
            send::interaction_edit(http, token, msg).await
        }
        Action::Setting(name) => {
            if let Err(r) = guard::admin_for(identity, user, ic.member.as_ref()) {
                return send::interaction_followup(http, token, r.view(&player.snapshot().await))
                    .await;
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
                return send::interaction_followup(http, token, r.view(&player.snapshot().await))
                    .await;
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

/// The equalizer panel for this guild as it stands, its curve drawn fresh.
pub async fn eq_panel(player: &GuildPlayer) -> Message {
    let snap = player.snapshot().await;
    let band = player.eq_band().await;
    let hex = format!("#{:06x}", snap.icons.accent());
    let cfg = snap.eq.clone();
    let picture = tokio::task::spawn_blocking(move || eq::picture(&cfg, &hex))
        .await
        .ok()
        .and_then(|r| r.ok());
    views::equalizer(&snap, band, picture)
}

/// The play log this guild's history pages, newest first.
async fn recent(identity: &Identity, guild: GuildId) -> anyhow::Result<Vec<PlayEntry>> {
    let Some(app_id) = identity.app_id_sync() else {
        return Ok(Vec::new());
    };
    Ok(settings::recent_plays(
        &identity.state.db,
        &app_id.to_string(),
        &guild.get().to_string(),
        views::HISTORY_LIMIT,
    )
    .await?)
}

/// A page of the current track's lyrics, or why there is none.
async fn lyrics_page(identity: &Identity, snap: &PlayerSnapshot, page: usize) -> Message {
    let Some(cur) = &snap.current else {
        return views::notice(
            snap,
            "Nothing playing",
            "-# Lyrics follow the current track.",
        );
    };
    let track = &cur.item.track;
    let raw = crate::discord::lyrics::text_for(&identity.state, track)
        .await
        .unwrap_or_default();
    let pages = crate::discord::lyrics::pages(
        &crate::discord::lyrics::lines(&raw),
        crate::discord::lyrics::PAGE_CHARS,
    );
    if pages.is_empty() {
        return views::notice(
            snap,
            "No lyrics",
            "-# Neither the file's tags nor Chordia have any.",
        );
    }
    views::lyrics(snap, track, &pages, page)
}

async fn controlled_action(action: &Action, player: &Arc<GuildPlayer>) -> Result<(), PlayerError> {
    match action {
        Action::PlayPause => player.toggle_pause().await.map(|_| ()),
        Action::Skip => player.skip().await.map(|_| ()),
        Action::Previous => player.previous().await.map(|_| ()),
        Action::Stop => player.stop().await,
        Action::Shuffle => {
            player.toggle_shuffle().await;
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
        Action::Mute => player.toggle_mute().await.map(|_| ()),
        Action::SeekBack | Action::SeekForward => {
            let snap = player.snapshot().await;
            let at = snap
                .current
                .as_ref()
                .map(|c| c.position_ms)
                .ok_or(PlayerError::NothingPlaying)?;
            let to = if matches!(action, Action::SeekBack) {
                at.saturating_sub(10_000)
            } else {
                at + 10_000
            };
            player.seek(Duration::from_millis(to)).await.map(|_| ())
        }
        Action::Clear => {
            player.clear().await;
            Ok(())
        }
        Action::Leave => {
            player.leave(LeaveReason::Command).await;
            Ok(())
        }
        _ => Ok(()),
    }
}
