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
        control::eq(),
        control::crossfade(),
        info::bots(),
        info::lyrics(),
        info::stats(),
        settings::settings(),
        settings::dj(),
        settings::skipmode(),
        settings::pickup(),
        settings::always_on(),
    ]
}

/// Discord's interface languages, as Discord names them. Each gets the command descriptions in
/// its language where the `discord` catalog has them (the catalog crate maps `de` to `de-DE`,
/// `ja` to `ja-JP`, and so on); the rest read the English.
const DISCORD_LOCALES: &[&str] = &[
    "id", "da", "de", "en-GB", "en-US", "es-ES", "es-419", "fr", "hr", "it", "lt", "hu", "nl",
    "no", "pl", "pt-BR", "ro", "fi", "sv-SE", "vi", "tr", "cs", "el", "bg", "ru", "uk", "hi", "th",
    "zh-CN", "ja", "zh-TW", "ko",
];

/// Discord's ceiling on a description, in characters.
const DESCRIPTION_MAX: usize = 100;

/// The commands with their descriptions localized from the `discord` catalog: a command's
/// description at `discord:commands.<name>.description`, a parameter's at
/// `discord:commands.<name>.params.<param>`. Names stay English so `/play` is `/play` everywhere.
pub fn localized(
    mut commands: Vec<poise::Command<Data, Error>>,
) -> Vec<poise::Command<Data, Error>> {
    for c in &mut commands {
        let base = format!("discord:commands.{}", c.name);
        fill(
            &mut c.description_localizations,
            &format!("{base}.description"),
        );
        for p in &mut c.parameters {
            fill(
                &mut p.description_localizations,
                &format!("{base}.params.{}", p.name),
            );
        }
    }
    commands
}

/// Every Discord locale's rendering of `key` that differs from the English, kept to Discord's
/// length. A key the catalog lacks, or a locale that falls back to English, adds nothing.
fn fill(into: &mut std::collections::HashMap<String, String>, key: &str) {
    let english = chordia_i18n::t0("en", key);
    if english == key {
        return;
    }
    for locale in DISCORD_LOCALES {
        let text = chordia_i18n::t0(locale, key);
        if text != key && text != english && text.chars().count() <= DESCRIPTION_MAX {
            into.insert((*locale).to_string(), text);
        }
    }
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

/// Does this bot serve the server the interaction came from? The allow list ships empty and an
/// empty list denies, so a bot someone invited off its public application id answers nothing
/// until the library owner allows that server in the dashboard. Everything the bot would say
/// goes through this: commands ([`allowed_guild`]), button presses
/// (`interactions::handle`) and the autocomplete lists, which are a catalog oracle of their own.
pub fn serves(ctx: Context<'_>) -> bool {
    ctx.guild_id()
        .is_some_and(|g| ctx.data().settings().allows_guild(g.get()))
}

/// Poise's check on every command. A refusal names the server id and where to allow it, so the
/// owner's own server is one paste away from working.
pub async fn allowed_guild(ctx: Context<'_>) -> Result<bool, Error> {
    let Some(guild) = ctx.guild_id() else {
        // Every command is `guild_only`; poise refuses a DM before this.
        return Ok(true);
    };
    if serves(ctx) {
        return Ok(true);
    }
    let identity = ctx.data();
    tracing::info!(
        bot = identity.index,
        guild = guild.get(),
        command = %ctx.command().qualified_name,
        "refusing a command from a server that is not on the allow list"
    );
    ctx.defer_ephemeral().await?;
    let snap = PlayerSnapshot::bare(identity).await;
    let msg = views::notice(
        &snap,
        "This server isn't allowed",
        &format!(
            "The library owner hasn't allowed this server, so I don't play here.\n-# They can add server ID `{}` under **Allowed servers** in the bot's settings.",
            guild.get()
        ),
    );
    send::respond(ctx, msg).await?;
    Ok(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog's English is the code's: a description edited in one place and not the
    /// other would hand translators a stale source.
    #[test]
    fn the_catalog_mirrors_every_description_in_the_code() {
        for c in all() {
            let key = format!("discord:commands.{}.description", c.name);
            assert_eq!(
                chordia_i18n::t0("en", &key),
                c.description.clone().unwrap_or_default(),
                "{key}"
            );
            for p in &c.parameters {
                let key = format!("discord:commands.{}.params.{}", c.name, p.name);
                assert_eq!(
                    chordia_i18n::t0("en", &key),
                    p.description.clone().unwrap_or_default(),
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn every_command_reads_in_the_translated_languages_and_within_discords_limit() {
        let commands = localized(all());
        for c in &commands {
            for locale in ["de", "fr", "es-ES", "pt-BR", "ja", "ko"] {
                let d = c
                    .description_localizations
                    .get(locale)
                    .unwrap_or_else(|| panic!("/{} has no {locale} description", c.name));
                assert!(
                    d.chars().count() <= DESCRIPTION_MAX,
                    "/{} {locale}: {d}",
                    c.name
                );
                // A parameter that is only a range ("0–150") reads the same in every language and
                // carries nothing; the rest are translated and within the limit.
                for p in &c.parameters {
                    if let Some(d) = p.description_localizations.get(locale) {
                        assert!(
                            d.chars().count() <= DESCRIPTION_MAX,
                            "/{} {}",
                            c.name,
                            p.name
                        );
                    } else {
                        assert!(
                            p.description
                                .as_deref()
                                .unwrap_or("")
                                .starts_with(char::is_numeric),
                            "/{} {} has no {locale} text",
                            c.name,
                            p.name
                        );
                    }
                }
            }
            // English-speaking regions read the source and carry nothing extra.
            assert!(!c.description_localizations.contains_key("en-GB"));
            assert!(!c.description_localizations.contains_key("en-US"));
            // Names stay English everywhere.
            assert!(c.name_localizations.is_empty());
        }
        // The registered set's hash covers the localizations, so a new translation re-registers.
        let plain =
            serde_json::to_string(&poise::builtins::create_application_commands(&all())).unwrap();
        let local = serde_json::to_string(&poise::builtins::create_application_commands(&commands))
            .unwrap();
        assert_ne!(plain, local);
    }
}
