//! Slash commands. Each file is one group; `all()` is the registered set.
//!
//! Every command follows the same shape: defer (public or ephemeral, decided up front because
//! Discord fixes visibility on the first response), run the guards in `guard.rs`, act on the guild's
//! player, and answer with a view from `ui::views` through `ui::send::respond`.

pub mod control;
pub mod guard;
pub mod info;
pub mod play;
pub mod queue;
pub mod settings;

use std::sync::Arc;

use serenity::all::GuildId;

use crate::discord::emoji::IconSet;
use crate::discord::identity::Identity;
use crate::discord::player::PlayerSnapshot;
use crate::discord::ui::{send, views};

pub type Data = Arc<Identity>;
pub type Error = anyhow::Error;
pub type Context<'a> = poise::Context<'a, Data, Error>;

pub fn all() -> Vec<poise::Command<Data, Error>> {
    vec![
        play::play(),
        play::search(),
        play::album(),
        play::artist(),
        play::playlist(),
        queue::queue(),
        queue::nowplaying(),
        queue::history(),
        queue::remove(),
        queue::move_track(),
        queue::jump(),
        queue::clear(),
        queue::shuffle(),
        control::skip(),
        control::forceskip(),
        control::vote(),
        control::back(),
        control::pause(),
        control::resume(),
        control::stop(),
        control::seek(),
        control::volume(),
        control::loop_mode(),
        control::join(),
        control::leave(),
        control::radio(),
        info::bots(),
        info::lyrics(),
        info::stats(),
        settings::settings(),
        settings::dj(),
        settings::skipmode(),
        settings::always_on(),
    ]
}

/// The bot's icon set, for views that have no player snapshot to take it from.
pub fn icons(ctx: Context<'_>) -> Arc<IconSet> {
    ctx.data().icons()
}

/// A snapshot for a reply that has no player of its own to take one from: the server's player
/// (made on the spot when the bot has not played there yet), so the reply is laid out by that
/// server's layouts with the bot's facts filled in. Outside a server, the bot's own.
pub async fn snap(ctx: Context<'_>) -> PlayerSnapshot {
    match ctx.guild_id() {
        Some(guild) => ctx.data().player(guild).await.snapshot().await,
        None => PlayerSnapshot::bare(ctx.data()).await,
    }
}

/// The guild a command ran in. Every command is `guild_only`, so this only fails for a DM that
/// slipped through.
pub fn guild_of(ctx: Context<'_>) -> anyhow::Result<GuildId> {
    ctx.guild_id()
        .ok_or_else(|| anyhow::anyhow!("this command only works in a server"))
}

/// Framework-level error handling: a command that returned `Err` shows the error as an ephemeral
/// error view (best effort), and everything else goes through poise's default logging.
pub async fn on_error(error: poise::FrameworkError<'_, Data, Error>) {
    match error {
        poise::FrameworkError::Command { error, ctx, .. } => {
            tracing::warn!(command = %ctx.command().qualified_name, error = %error, "command failed");
            let msg = views::error(&snap(ctx).await, "Couldn't do that", &format!("-# {error}"));
            if let Err(e) = send::respond(ctx, msg).await {
                tracing::debug!(error = %e, "reporting a command error");
            }
        }
        poise::FrameworkError::CommandPanic { payload, ctx, .. } => {
            tracing::error!(command = %ctx.command().qualified_name, payload = ?payload, "command panicked");
            let msg = views::error(
                &snap(ctx).await,
                "Something broke",
                "-# The library logged it.",
            );
            let _ = send::respond(ctx, msg).await;
        }
        other => {
            if let Err(e) = poise::builtins::on_error(other).await {
                tracing::warn!(error = %e, "poise error handler failed");
            }
        }
    }
}
