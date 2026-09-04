//! `/bots`, `/lyrics` and `/stats`.

use super::{guard, Context, Error};
use crate::discord::identity::Status;
use crate::discord::lyrics;
use crate::discord::settings;
use crate::discord::ui::{fmt, send, views};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum StatsScope {
    #[name = "server"]
    Server,
    #[name = "me"]
    Me,
}

/// The last thirty days, in milliseconds.
const STATS_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// What this server has played through the bot in the last 30 days
#[poise::command(slash_command, guild_only)]
pub async fn stats(
    ctx: Context<'_>,
    #[description = "Everyone's plays, or only what you asked for"] scope: Option<StatsScope>,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = super::guild_of(ctx)?;
    let identity = ctx.data();
    let icons = super::icons(ctx);
    let Some(app_id) = identity.app_id_sync() else {
        return send::respond(ctx, guard::Refusal::Offline.view(&icons)).await;
    };
    let me = matches!(scope, Some(StatsScope::Me));
    let stats = settings::guild_stats(
        &identity.state.db,
        &app_id.to_string(),
        &guild.get().to_string(),
        settings::now_ms() - STATS_WINDOW_MS,
        me.then(|| ctx.author().id.get()),
    )
    .await?;
    if stats.plays == 0 {
        return send::respond(
            ctx,
            views::notice(
                &icons,
                "Nothing played yet",
                "-# Stats cover the last 30 days in this server.",
            ),
        )
        .await;
    }
    let mut sections = vec![(
        "Listening".to_string(),
        format!(
            "{} · {}",
            fmt::count(stats.plays as usize, "play"),
            fmt::duration(stats.ms.max(0) as u64)
        ),
    )];
    if !stats.top_tracks.is_empty() {
        let lines: Vec<String> = stats
            .top_tracks
            .iter()
            .enumerate()
            .map(|(i, (title, artist, plays))| {
                format!(
                    "{}. **{}** · {} · {}",
                    i + 1,
                    fmt::escape_md(title),
                    fmt::escape_md(artist),
                    fmt::count(*plays as usize, "play")
                )
            })
            .collect();
        sections.push(("Most played".to_string(), lines.join("\n")));
    }
    if !me && !stats.top_requesters.is_empty() {
        let lines: Vec<String> = stats
            .top_requesters
            .iter()
            .map(|(user, plays)| format!("<@{user}> · {}", fmt::count(*plays as usize, "play")))
            .collect();
        sections.push(("Top requesters".to_string(), lines.join("\n")));
    }
    send::respond(
        ctx,
        views::stats(
            &icons,
            if me {
                "Your plays"
            } else {
                "This server's plays"
            },
            "Last 30 days, through this bot",
            &sections,
        ),
    )
    .await
}

/// Lyrics for the track that is playing, from the file's own tags
#[poise::command(slash_command, guild_only)]
pub async fn lyrics(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = super::guild_of(ctx)?;
    let identity = ctx.data();
    let Some(player) = identity.player_arc(guild) else {
        return send::respond(
            ctx,
            views::notice(
                &super::icons(ctx),
                "Nothing playing",
                "-# Lyrics follow the current track.",
            ),
        )
        .await;
    };
    let snap = player.snapshot().await;
    let Some(cur) = &snap.current else {
        return send::respond(
            ctx,
            views::notice(
                &super::icons(ctx),
                "Nothing playing",
                "-# Lyrics follow the current track.",
            ),
        )
        .await;
    };
    let track = &cur.item.track;
    let raw = crate::catalog::get_track_lyrics(&identity.state.db, &track.id)
        .await?
        .unwrap_or_default();
    let pages = lyrics::pages(&lyrics::lines(&raw), lyrics::PAGE_CHARS);
    send::respond(
        ctx,
        views::lyrics(&snap, &track.title, &track.artist, &pages, 0),
    )
    .await
}

/// See every bot identity and which are free
#[poise::command(slash_command, guild_only)]
pub async fn bots(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let guild = super::guild_of(ctx)?;
    let identities = match crate::discord::runtime() {
        Some(rt) => rt.identities.clone(),
        None => vec![ctx.data().clone()],
    };
    let mut lines = Vec::with_capacity(identities.len());
    for id in identities {
        let (playing_in, listeners) = match id.player_arc(guild) {
            Some(p) => {
                let snap = p.snapshot().await;
                (snap.voice_channel, snap.listeners)
            }
            None => (None, 0),
        };
        lines.push(views::BotLine {
            name: id.display_name_sync(),
            online: id.status() == Status::Online,
            playing_in,
            listeners,
        });
    }
    send::respond(ctx, views::bots(&super::icons(ctx), &lines)).await
}
