//! Discord music bot(s), playing this library into voice channels.
//!
//! ## Shape
//!
//! Every token in `[discord]` becomes an [`Identity`]: an independent bot with its own slash
//! commands, queue per guild, voice connection and presence. Identities share this library's
//! catalog and one songbird mixer-thread pool, and they know about each other, so a bot that is
//! busy in one voice channel can point a second channel at a sibling that is free (the Jockie Music
//! model, with the library as the only source).
//!
//! ## Crate choices, and why
//!
//! - **serenity 0.12 + poise 0.6** for the gateway and slash commands. serenity 0.12 predates
//!   Discord's Components V2, so every message body is built by [`ui::v2`] as JSON and sent through
//!   serenity's raw `Http` methods, which accept any `Serialize`. serenity skips unknown top-level
//!   component kinds when it parses messages back, so the two coexist.
//! - **songbird 0.6** for voice. It implements DAVE — Discord's voice end-to-end encryption,
//!   mandatory since March 2026 — in pure Rust, and decodes with Symphonia, which this crate already
//!   ships with every codec enabled. Its one C dependency is libopus (`opus2` → `libopus_sys`),
//!   found through pkg-config or built from source with cmake. On Windows with more than one Visual
//!   Studio installed, cmake may need `CMAKE_GENERATOR=Ninja`.
//! - **resvg** for everything drawn: the application emoji set (Phosphor icons and the progress-bar
//!   segments in the bot's colour), the avatar (the Chordia mark in that colour), and later the
//!   now-playing card.
//!
//! The whole module is behind the `discord` cargo feature (on by default) so the desktop app, which
//! embeds this crate, never builds any of it.

pub mod avatar;
pub mod client;
pub mod commands;
pub mod emoji;
pub mod identity;
pub mod interactions;
pub mod lyrics;
pub mod player;
pub mod presence;
pub mod rest;
pub mod settings;
pub mod source;
pub mod theme;
pub mod ui;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serenity::all::GuildId;
use songbird::driver::Scheduler;
use tokio_util::sync::CancellationToken;

use crate::http::AppState;
pub use identity::{Identity, Status};

static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();

/// Every identity in this process, and the switch that stops them all.
pub struct Runtime {
    pub identities: Vec<Arc<Identity>>,
    cancel: CancellationToken,
}

/// The running bots, if any were configured.
pub fn runtime() -> Option<Arc<Runtime>> {
    RUNTIME.get().cloned()
}

/// Start one bot per configured token. Returns `None` (and starts nothing) when no token is set.
/// Idempotent: a second call returns the runtime the first created.
pub fn start(state: AppState) -> Option<Arc<Runtime>> {
    let tokens: Vec<String> = state
        .config
        .discord
        .tokens()
        .into_iter()
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return None;
    }
    if let Some(existing) = RUNTIME.get() {
        return Some(existing.clone());
    }
    let cancel = CancellationToken::new();
    let scheduler = Scheduler::default();
    let command_guilds = state.config.discord.command_guilds.clone();
    let identities: Vec<Arc<Identity>> = tokens
        .into_iter()
        .enumerate()
        .map(|(i, token)| {
            Identity::new(
                i as u8,
                token,
                state.clone(),
                scheduler.clone(),
                cancel.clone(),
                command_guilds.clone(),
            )
        })
        .collect();
    let runtime = Arc::new(Runtime { identities, cancel });
    let runtime = match RUNTIME.set(runtime.clone()) {
        Ok(()) => runtime,
        Err(_) => return RUNTIME.get().cloned(),
    };
    tracing::info!(bots = runtime.identities.len(), "starting Discord bot(s)");
    for identity in &runtime.identities {
        client::spawn_supervisor(identity.clone());
    }
    Some(runtime)
}

impl Runtime {
    pub fn identity(&self, index: u8) -> Option<Arc<Identity>> {
        self.identities.get(index as usize).cloned()
    }

    /// Identities that are online and not in a voice channel of `guild`, other than `except`.
    pub async fn free_siblings(&self, guild: GuildId, except: u8) -> Vec<Arc<Identity>> {
        let mut out = Vec::new();
        for id in &self.identities {
            if id.index == except || id.status() != Status::Online {
                continue;
            }
            if !id.settings().allows_guild(guild.get()) {
                continue;
            }
            let busy = match id.player_arc(guild) {
                Some(p) => p.voice_channel().await.is_some(),
                None => false,
            };
            if !busy {
                out.push(id.clone());
            }
        }
        out
    }

    /// Leave every voice channel cleanly and close every gateway. Bounded: Discord may be slow or
    /// gone, and a shutdown must not hang on it.
    pub async fn shutdown(&self) {
        tracing::info!("stopping Discord bot(s)");
        let work = async {
            for identity in &self.identities {
                identity.set_status(Status::Stopped);
                for player in identity.players() {
                    player.leave(player::LeaveReason::Shutdown).await;
                }
                if let Some(sm) = identity.shard_manager() {
                    sm.shutdown_all().await;
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(5), work)
            .await
            .is_err()
        {
            tracing::warn!("Discord shutdown did not finish within 5s; continuing");
        }
        self.cancel.cancel();
    }
}
