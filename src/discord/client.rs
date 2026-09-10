//! The gateway connection for one identity, and the supervisor that keeps it alive.
//!
//! `run_once` builds a serenity client with the poise framework and a fresh songbird, validates the
//! token by asking Discord who it is, and runs the gateway until it exits — on its own, because the
//! runtime is shutting down, or because the dashboard asked for a restart. The supervisor wraps that
//! in a retry loop with exponential backoff, so a bad token or a Discord outage is a logged, visible
//! `failed` status rather than a crashed library.

use std::sync::Arc;
use std::time::Duration;

use serenity::all::{FullEvent, GatewayIntents, GuildId, Interaction, Ready};
use sha2::{Digest, Sha256};
use songbird::Songbird;

use crate::discord::commands::{self, Data, Error};
use crate::discord::identity::{Identity, Profile, Status};
use crate::discord::{interactions, presence};

/// Everything the bot asks the gateway for. All three are ordinary intents: nothing here needs a
/// toggle in the Developer Portal, and nothing here is privileged.
///
/// - `GUILDS` — the guild, channel and role events that fill the cache. That is how the bot knows
///   a channel's name and configured bitrate (the Opus bitrate it encodes at), which channels are
///   voice channels, and when it has been added to a guild (the allow-list check on join).
/// - `GUILD_VOICE_STATES` — `VOICE_STATE_UPDATE`. songbird needs the bot's own voice state to
///   complete a join, and every guard needs everyone else's: which channel the caller is in, who
///   is listening (the alone-in-channel DJ rule), and when the channel empties (idle leave).
/// - `GUILD_MESSAGES` — `MESSAGE_CREATE`, used for exactly one thing: counting how many messages
///   have landed under the now-playing controller, so it can re-post itself at the bottom once it
///   has scrolled away. Message *content* is never requested, so the events arrive without text,
///   which is all the counter needs.
///
/// Deliberately absent: `MESSAGE_CONTENT` (privileged; slash commands and buttons carry their data
/// in the interaction), `GUILD_MEMBERS` (privileged; interaction payloads already include the
/// caller's roles and permissions, and voice-state events carry the member), `GUILD_PRESENCES`
/// (privileged; nothing here cares what anyone else is doing). Interactions and
/// `VOICE_SERVER_UPDATE` are not gated by any intent.
///
/// The invite must also grant the permissions in [`Identity::INVITE_PERMISSIONS`] and the
/// `bot` + `applications.commands` scopes; [`Identity::invite_url`] builds that link.
pub const INTENTS: GatewayIntents = GatewayIntents::GUILDS
    .union(GatewayIntents::GUILD_VOICE_STATES)
    .union(GatewayIntents::GUILD_MESSAGES);

const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// How often each player checks idle timers and refreshes its progress line. Five seconds keeps a
/// 12-segment bar moving on a typical track (24 positions over 3–4 minutes) at well under Discord's
/// edit budget per channel; edits are coalesced by the controller task anyway.
const TICK: Duration = Duration::from_secs(5);

enum Exit {
    /// The runtime is shutting down.
    Cancelled,
    /// A restart was requested; reconnect immediately.
    Restart,
    /// The gateway ended on its own.
    Ended,
}

pub fn spawn_supervisor(identity: Arc<Identity>) {
    tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN;
        loop {
            if identity.cancel.is_cancelled() {
                break;
            }
            identity.set_status(Status::Connecting);
            let outcome = run_once(&identity).await;
            identity.detach_connection();
            for player in identity.players() {
                player.on_disconnected().await;
            }
            match outcome {
                Ok(Exit::Cancelled) => break,
                Ok(Exit::Restart) => {
                    tracing::info!(bot = identity.index, "restarting Discord bot");
                    backoff = BACKOFF_MIN;
                    continue;
                }
                Ok(Exit::Ended) => {
                    tracing::warn!(bot = identity.index, "Discord gateway ended; reconnecting");
                    identity.set_status(Status::Failed("gateway closed".into()));
                }
                Err(e) => {
                    tracing::error!(bot = identity.index, error = %e, "Discord bot failed; will retry");
                    identity.set_status(Status::Failed(e.to_string()));
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = identity.cancel.cancelled() => break,
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        identity.set_status(Status::Stopped);
    });
}

async fn run_once(identity: &Arc<Identity>) -> anyhow::Result<Exit> {
    let intents = INTENTS;

    let data: Data = identity.clone();
    let setup_data = data.clone();
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::localized(commands::all()),
            event_handler: |ctx, event, fw, data| Box::pin(on_event(ctx, event, fw, data)),
            on_error: |e| Box::pin(commands::on_error(e)),
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                on_ready(&setup_data, ctx, ready, framework).await;
                Ok(setup_data)
            })
        })
        .build();

    let songbird_config = songbird::Config::default().scheduler(identity.scheduler.clone());
    let songbird = Songbird::serenity_from_config(songbird_config);

    let mut client = serenity::Client::builder(&identity.token, intents)
        .framework(framework)
        .voice_manager_arc(songbird.clone())
        .await?;

    // Ask who we are before opening the gateway: this is where a bad token fails, with a clear
    // HTTP 401 rather than a gateway close code.
    let app = client.http.get_current_application_info().await?;
    let me = client.http.get_current_user().await?;
    // The application-emoji routes are addressed by application id; serenity only learns it from
    // the gateway's Ready, and this runs first.
    client.http.set_application_id(app.id);
    identity.set_profile(Profile {
        app_id: app.id.get(),
        user_id: me.id,
        name: me.name.clone(),
        avatar_url: me.avatar_url(),
    });
    identity.load_settings().await;
    identity.attach_connection(
        client.http.clone(),
        client.cache.clone(),
        songbird,
        client.shard_manager.clone(),
    );
    tracing::info!(bot = identity.index, app_id = app.id.get(), name = %me.name, "Discord bot connecting");

    let ticker = spawn_ticker(identity.clone());
    let shard_manager = client.shard_manager.clone();
    let exit = tokio::select! {
        r = client.start() => {
            r?;
            Exit::Ended
        }
        _ = identity.cancel.cancelled() => {
            shard_manager.shutdown_all().await;
            Exit::Cancelled
        }
        _ = identity.restart_requested() => {
            shard_manager.shutdown_all().await;
            Exit::Restart
        }
    };
    ticker.abort();
    Ok(exit)
}

fn spawn_ticker(identity: Arc<Identity>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let idle = Duration::from_secs(identity.settings().idle_timeout_secs.max(30) as u64);
            for player in identity.players() {
                player.tick(idle).await;
            }
            // Anything the theme job could not do earlier (a rate limit, a dashboard change while
            // offline) gets its chance here.
            crate::discord::theme::tick(&identity).await;
            // Rotating statuses and live counts advance here; unchanged text is not resent.
            presence::update(&identity).await;
        }
    })
}

async fn on_ready(
    identity: &Arc<Identity>,
    ctx: &serenity::all::Context,
    ready: &Ready,
    framework: &poise::Framework<Data, Error>,
) {
    identity.set_context(ctx.clone());
    if let Some(mut p) = identity.profile() {
        p.name = ready.user.name.clone();
        p.avatar_url = ready.user.avatar_url();
        identity.set_profile(p);
    }
    register_commands(identity, ctx, framework).await;
    // The look: icon set and avatar in the bot's colour. One listing on a normal boot, a batch of
    // uploads on the first one or after a colour change. Bounded so a slow Discord cannot hold the
    // bot in "connecting"; whatever is left over, the ticker finishes.
    if tokio::time::timeout(
        Duration::from_secs(120),
        crate::discord::theme::tick(identity),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            bot = identity.index,
            "theme setup timed out; the ticker will finish it"
        );
    }
    identity.set_status(Status::Online);
    tracing::info!(
        bot = identity.index,
        guilds = ready.guilds.len(),
        "Discord bot online"
    );
    presence::update(identity).await;
    rejoin_always_on(identity).await;
}

/// After a restart, every server the bot was busy in gets it back as it was (its restart
/// record), and 24/7 servers with nothing recorded get it back in the channel they keep.
async fn rejoin_always_on(identity: &Arc<Identity>) {
    let Some(app_id) = identity.app_id_sync() else {
        return;
    };
    let db = &identity.state.db;
    let app = app_id.to_string();
    let mut restored = std::collections::HashSet::new();
    for (guild_id, json) in crate::discord::settings::load_queues(db, &app)
        .await
        .unwrap_or_default()
    {
        let Some(guild) = guild_id.parse::<u64>().ok().map(GuildId::new) else {
            continue;
        };
        let player = identity.player(guild).await;
        if player.restore(&json).await {
            restored.insert(guild);
        } else {
            // A channel that is gone stays gone; the record would only fail again.
            let _ = crate::discord::settings::clear_queue(db, &app, &guild_id).await;
        }
    }
    let guilds = match crate::discord::settings::load_guilds(db, &app).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "loading guild settings for 24/7 rejoin");
            return;
        }
    };
    for gs in guilds.into_iter().filter(|g| g.always_on) {
        let (Some(guild), Some(voice)) = (
            gs.guild_id.parse::<u64>().ok(),
            gs.always_on_channel_id
                .as_deref()
                .and_then(|c| c.parse::<u64>().ok()),
        ) else {
            continue;
        };
        let guild = GuildId::new(guild);
        if restored.contains(&guild) {
            continue;
        }
        let voice = serenity::all::ChannelId::new(voice);
        let text = gs
            .controller_channel_id
            .as_deref()
            .and_then(|c| c.parse::<u64>().ok())
            .map(serenity::all::ChannelId::new)
            .unwrap_or(voice);
        let player = identity.player(guild).await;
        match player.join(voice, text).await {
            Ok(()) => tracing::info!(
                bot = identity.index,
                guild = guild.get(),
                "rejoined 24/7 channel"
            ),
            Err(e) => {
                tracing::warn!(bot = identity.index, guild = guild.get(), error = %e, "24/7 rejoin failed")
            }
        }
    }
}

/// Register slash commands — globally, once, only when the command set changed since the last
/// registration (Discord rate-limits this and global commands take up to an hour to propagate); or
/// per guild on every boot when `command_guilds` is set for development.
async fn register_commands(
    identity: &Arc<Identity>,
    ctx: &serenity::all::Context,
    framework: &poise::Framework<Data, Error>,
) {
    let commands = &framework.options().commands;
    let builders = poise::builtins::create_application_commands(commands);
    let hash = serde_json::to_vec(&builders)
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
        .unwrap_or_default();

    if !identity.command_guilds.is_empty() {
        for g in &identity.command_guilds {
            if let Err(e) =
                poise::builtins::register_in_guild(&ctx.http, commands, GuildId::new(*g)).await
            {
                tracing::warn!(bot = identity.index, guild = g, error = %e, "registering guild commands");
            }
        }
        return;
    }

    let mut settings = identity.settings();
    if settings.commands_hash.as_deref() == Some(hash.as_str()) {
        return;
    }
    match poise::builtins::register_globally(&ctx.http, commands).await {
        Ok(()) => {
            tracing::info!(
                bot = identity.index,
                commands = commands.len(),
                "registered global slash commands"
            );
            settings.commands_hash = Some(hash);
            identity.set_settings(settings);
            identity.save_settings().await;
        }
        Err(e) => tracing::warn!(bot = identity.index, error = %e, "registering global commands"),
    }
}

async fn on_event(
    ctx: &serenity::all::Context,
    event: &FullEvent,
    _framework: poise::FrameworkContext<'_, Data, Error>,
    identity: &Data,
) -> Result<(), Error> {
    match event {
        FullEvent::VoiceStateUpdate { old, new } => {
            let Some(guild) = new
                .guild_id
                .or_else(|| old.as_ref().and_then(|o| o.guild_id))
            else {
                return Ok(());
            };
            let Some(player) = identity.player_arc(guild) else {
                return Ok(());
            };
            if Some(new.user_id) == identity.user_id() {
                match new.channel_id {
                    None => player.on_disconnected().await,
                    Some(ch) => player.moved_to(ch).await,
                }
            }
            identity.refresh_listeners(&player).await;
        }
        FullEvent::InteractionCreate {
            interaction: Interaction::Component(ic),
        } => {
            if let Err(e) = interactions::handle(identity, ctx, ic).await {
                tracing::warn!(bot = identity.index, custom_id = %ic.data.custom_id, error = %e, "component interaction failed");
            }
        }
        FullEvent::ChannelUpdate { new, .. } => {
            if let Some(player) = identity.player_arc(new.guild_id) {
                player
                    .refresh_bitrate(new.id, new.bitrate.map(|b| b / 1000))
                    .await;
            }
        }
        FullEvent::Message { new_message } => {
            if let Some(guild) = new_message.guild_id {
                if let Some(player) = identity.player_arc(guild) {
                    player.note_channel_message(new_message.channel_id).await;
                }
            }
        }
        FullEvent::GuildCreate { guild, .. }
            if !identity.settings().allows_guild(guild.id.get()) =>
        {
            tracing::info!(
                bot = identity.index,
                guild = guild.id.get(),
                "leaving a guild that is not on the allow list"
            );
            if let Err(e) = guild.id.leave(&ctx.http).await {
                tracing::warn!(error = %e, "leaving guild");
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portal toggles stay off: none of the privileged intents may ever creep into this set.
    #[test]
    fn no_privileged_intents() {
        assert!(!INTENTS.contains(GatewayIntents::MESSAGE_CONTENT));
        assert!(!INTENTS.contains(GatewayIntents::GUILD_MEMBERS));
        assert!(!INTENTS.contains(GatewayIntents::GUILD_PRESENCES));
        assert!(INTENTS.contains(GatewayIntents::GUILD_VOICE_STATES));
    }
}
