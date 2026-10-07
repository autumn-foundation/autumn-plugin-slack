//! Example Slack app: a mention reply, a slash command, a button, a modal,
//! and the Home tab.
//!
//! Set `SLACK_SIGNING_SECRET` and `SLACK_BOT_TOKEN`. Then:
//! `cargo run --example app`
//!
//! In the Slack app config, set these request URLs (`https://<host>` is your
//! public URL, for example from a tunnel):
//! - Event Subscriptions: `https://<host>/slack/events`
//! - Slash Commands (`/deploy`): `https://<host>/slack/commands`
//! - Interactivity: `https://<host>/slack/interactions`

use autumn_plugin_slack::payload::{
    BlockAction, CommandResponse, EventCallback, Message, MessageEvent, SlashCommand, ViewPayload,
    ViewResponse,
};
use autumn_plugin_slack::{HandlerResult, SlackContext, SlackPlugin};
use serde_json::json;

/// Replies in the thread of a mention.
async fn on_mention(ctx: SlackContext, ev: EventCallback) -> HandlerResult<()> {
    let msg: MessageEvent = ev.parse()?;
    // A reply in a thread uses the parent `ts`.
    let (Some(channel), Some(ts)) = (msg.channel, msg.thread_ts.or(msg.ts)) else {
        return Ok(());
    };
    ctx.client()
        .chat_post_message(&channel, &Message::text("Hi! Try /deploy.").in_thread(ts))
        .await?;
    Ok(())
}

/// Publishes the Home tab.
async fn on_home_opened(ctx: SlackContext, ev: EventCallback) -> HandlerResult<()> {
    let Some(user) = ev.event.get("user").and_then(|u| u.as_str()) else {
        return Ok(());
    };
    let view = json!({
        "type": "home",
        "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "*Deploy bot* is ready."}}],
    });
    ctx.client().views_publish(user, &view).await?;
    Ok(())
}

/// `/deploy <service>`: asks for approval with a button.
async fn deploy(_ctx: SlackContext, cmd: SlashCommand) -> HandlerResult<CommandResponse> {
    let service = if cmd.text.trim().is_empty() {
        "api"
    } else {
        cmd.text.trim()
    };
    let blocks = vec![
        json!({"type": "section", "text": {"type": "mrkdwn", "text": format!("Deploy *{service}*?")}}),
        json!({"type": "actions", "elements": [
            {"type": "button", "action_id": "approve_deploy", "value": service,
             "text": {"type": "plain_text", "text": "Approve"}, "style": "primary"},
            {"type": "button", "action_id": "open_notes",
             "text": {"type": "plain_text", "text": "Add notes"}},
        ]}),
    ];
    Ok(CommandResponse::in_channel(
        Message::text(format!("Deploy {service}?")).blocks(blocks),
    ))
}

/// The approve button replaces the question.
async fn approve(_ctx: SlackContext, action: BlockAction) -> HandlerResult<Option<Message>> {
    let service = action.value.unwrap_or_default();
    let user = action.user_id.unwrap_or_default();
    Ok(Some(
        Message::text(format!("<@{user}> approved the deploy of {service}.")).replace_original(),
    ))
}

/// The notes button opens a modal. A `trigger_id` is valid for 3 s.
async fn open_notes(ctx: SlackContext, action: BlockAction) -> HandlerResult<Option<Message>> {
    let Some(trigger) = action.trigger_id else {
        return Ok(None);
    };
    let view = json!({
        "type": "modal",
        "callback_id": "deploy_notes",
        "title": {"type": "plain_text", "text": "Deploy notes"},
        "submit": {"type": "plain_text", "text": "Save"},
        "blocks": [{"type": "input", "block_id": "notes",
            "label": {"type": "plain_text", "text": "Notes"},
            "element": {"type": "plain_text_input", "action_id": "value", "multiline": true}}],
    });
    ctx.client().views_open(&trigger, &view).await?;
    Ok(None)
}

/// The modal submit checks the input.
async fn save_notes(_ctx: SlackContext, view: ViewPayload) -> HandlerResult<ViewResponse> {
    let text = view
        .state_values()
        .and_then(|v| v.pointer("/notes/value/value"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if text.len() < 5 {
        return Ok(ViewResponse::errors([(
            "notes",
            "Write 5 characters or more.",
        )]));
    }
    tracing::info!(len = text.len(), "deploy notes saved");
    Ok(ViewResponse::Close)
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(
            SlackPlugin::new()
                .on_event("app_mention", on_mention)
                .on_event("app_home_opened", on_home_opened)
                .command("/deploy", deploy)
                .action("approve_deploy", approve)
                .action("open_notes", open_notes)
                .view_submission("deploy_notes", save_notes),
        )
        .run()
        .await;
}
