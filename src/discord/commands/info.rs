//! `/bots` — which identities exist and what each is doing here.

use super::{Context, Error};
use crate::discord::identity::Status;
use crate::discord::ui::{send, views};

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
                (
                    snap.voice_channel
                        .map(|c| id.channel_name(guild, c).unwrap_or_else(|| "voice".into())),
                    snap.listeners,
                )
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
    send::respond(ctx, views::bots(&lines)).await
}
