//! Delivering a [`Message`] through serenity's raw HTTP methods.
//!
//! Every method here takes a V2 [`Message`], validates it (a malformed tree is a bug, and a
//! `debug_assert` catches it in tests while release builds log and send anyway), and hands the
//! JSON plus any files to serenity, which does the multipart encoding and the rate limiting.
//!
//! Interaction responses: a slash command is answered by deferring first (poise does that) and then
//! editing the original response, because Discord only allows the `EPHEMERAL` flag on a deferred
//! initial response — the V2 flag has to arrive on the edit. A component press is answered in one
//! shot: `UPDATE_MESSAGE` (7) to replace the message it came from, `CHANNEL_MESSAGE_WITH_SOURCE` (4)
//! for a new (usually ephemeral) reply, or `DEFERRED_UPDATE_MESSAGE` (6) to acknowledge and let the
//! controller task do the redraw.

use serde_json::json;
use serenity::all::{ChannelId, ComponentInteraction, Http, MessageId};

use super::v2::Message;
use crate::discord::commands::Context;

fn check(msg: &Message) {
    if let Err(e) = msg.validate() {
        debug_assert!(false, "{e}");
        tracing::error!(error = %e, "sending an invalid components v2 message");
    }
}

/// Answer a slash command. Poise has already deferred it, so this edits the original response;
/// if it has not (a fast path that never awaited anything), send the initial response directly.
pub async fn respond(ctx: Context<'_>, mut msg: Message) -> anyhow::Result<()> {
    check(&msg);
    let poise::Context::Application(app) = ctx else {
        anyhow::bail!("prefix commands are not supported");
    };
    let files = msg.take_attachments();
    let body = msg.body();
    if app
        .has_sent_initial_response
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        app.serenity_context
            .http
            .edit_original_interaction_response(&app.interaction.token, &body, files)
            .await?;
    } else {
        app.serenity_context
            .http
            .create_interaction_response(
                app.interaction.id,
                &app.interaction.token,
                &json!({ "type": 4, "data": body }),
                files,
            )
            .await?;
        app.has_sent_initial_response
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(())
}

/// A second message after the response (e.g. a public "Queued" toast after an ephemeral picker).
pub async fn followup(ctx: Context<'_>, mut msg: Message) -> anyhow::Result<()> {
    check(&msg);
    let poise::Context::Application(app) = ctx else {
        anyhow::bail!("prefix commands are not supported");
    };
    let files = msg.take_attachments();
    let body = msg.body();
    app.serenity_context
        .http
        .create_followup_message(&app.interaction.token, &body, files)
        .await?;
    Ok(())
}

/// Replace the message a component belongs to.
pub async fn component_update(
    http: &Http,
    interaction: &ComponentInteraction,
    mut msg: Message,
) -> anyhow::Result<()> {
    check(&msg);
    let files = msg.take_attachments();
    let body = msg.body();
    http.create_interaction_response(
        interaction.id,
        &interaction.token,
        &json!({ "type": 7, "data": body }),
        files,
    )
    .await?;
    Ok(())
}

/// Reply to a component press with a new message (ephemeral unless the view says otherwise).
pub async fn component_reply(
    http: &Http,
    interaction: &ComponentInteraction,
    mut msg: Message,
) -> anyhow::Result<()> {
    check(&msg);
    let files = msg.take_attachments();
    let body = msg.body();
    http.create_interaction_response(
        interaction.id,
        &interaction.token,
        &json!({ "type": 4, "data": body }),
        files,
    )
    .await?;
    Ok(())
}

/// Acknowledge a component press without changing anything (the controller redraws itself).
pub async fn component_ack(http: &Http, interaction: &ComponentInteraction) -> anyhow::Result<()> {
    http.create_interaction_response(
        interaction.id,
        &interaction.token,
        &json!({ "type": 6 }),
        Vec::new(),
    )
    .await?;
    Ok(())
}

pub async fn post(
    http: &Http,
    channel: ChannelId,
    mut msg: Message,
) -> anyhow::Result<serenity::all::Message> {
    check(&msg);
    let files = msg.take_attachments();
    let body = msg.body();
    Ok(http.send_message(channel, files, &body).await?)
}

pub async fn edit(
    http: &Http,
    channel: ChannelId,
    message: MessageId,
    mut msg: Message,
) -> anyhow::Result<serenity::all::Message> {
    check(&msg);
    let files = msg.take_attachments();
    let body = msg.body();
    Ok(http.edit_message(channel, message, &body, files).await?)
}

pub async fn delete(http: &Http, channel: ChannelId, message: MessageId) -> anyhow::Result<()> {
    http.delete_message(channel, message, None).await?;
    Ok(())
}
