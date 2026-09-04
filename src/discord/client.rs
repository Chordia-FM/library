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

const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// How often each player checks idle timers and refreshes its progress line.
const TICK: Duration = Duration::from_secs(15);

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
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_VOICE_STATES
        // Only to count messages under the controller, so it can re-post when it scrolls away.
        | GatewayIntents::GUILD_MESSAGES;

    let data: Data = identity.clone();
    let setup_data = data.clone();
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::all(),
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
    identity.set_status(Status::Online);
    tracing::info!(
        bot = identity.index,
        guilds = ready.guilds.len(),
        "Discord bot online"
    );
    presence::update(identity).await;
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
