//! Typed Slack payloads and replies.
//!
//! `EventCallback` (in `event`), `BlockAction`, `ViewPayload`, and `Shortcut`
//! (in `payload`) keep the raw JSON. A handler can read fields that the
//! typed view does not have.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// ------------------------------------------------------------------ events

/// An Events API `event_callback`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[non_exhaustive]
pub struct EventCallback {
    /// Workspace ID.
    #[serde(default)]
    pub team_id: Option<String>,
    /// App ID.
    #[serde(default)]
    pub api_app_id: Option<String>,
    /// Unique event ID. Slack retries use the same ID.
    pub event_id: String,
    /// Event time, Unix seconds.
    #[serde(default)]
    pub event_time: Option<i64>,
    /// The inner event.
    pub event: Value,
    /// `X-Slack-Retry-Num` of this delivery. `None` on the first delivery.
    #[serde(skip)]
    pub retry_num: Option<u32>,
    /// `X-Slack-Retry-Reason`, for example `http_timeout`.
    #[serde(skip)]
    pub retry_reason: Option<String>,
}

impl EventCallback {
    /// The inner event type, for example `app_mention`.
    #[must_use]
    pub fn event_type(&self) -> &str {
        self.event.get("type").and_then(Value::as_str).unwrap_or("")
    }

    /// Decodes the inner event.
    ///
    /// # Errors
    /// Returns the serde error when the event does not match `T`.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.event)
    }
}

/// Common fields of `message` and `app_mention` events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct MessageEvent {
    /// Event type.
    #[serde(rename = "type")]
    pub kind: String,
    /// Message subtype, for example `bot_message`.
    pub subtype: Option<String>,
    /// Channel ID.
    pub channel: Option<String>,
    /// Channel type: `channel`, `group`, `im`, or `mpim`.
    pub channel_type: Option<String>,
    /// Author user ID.
    pub user: Option<String>,
    /// Bot ID, for bot messages.
    pub bot_id: Option<String>,
    /// Message text.
    pub text: Option<String>,
    /// Message timestamp (its ID).
    pub ts: Option<String>,
    /// Parent message timestamp, for a thread reply.
    pub thread_ts: Option<String>,
}

// --------------------------------------------------------------- commands

/// A slash command request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct SlashCommand {
    /// Command name with the slash, for example `/deploy`.
    pub command: String,
    /// Text after the command.
    #[serde(default)]
    pub text: String,
    /// User ID.
    #[serde(default)]
    pub user_id: String,
    /// User name (legacy).
    #[serde(default)]
    pub user_name: String,
    /// Channel ID.
    #[serde(default)]
    pub channel_id: String,
    /// Channel name.
    #[serde(default)]
    pub channel_name: String,
    /// Workspace ID.
    #[serde(default)]
    pub team_id: String,
    /// Workspace domain.
    #[serde(default)]
    pub team_domain: String,
    /// Enterprise Grid ID.
    #[serde(default)]
    pub enterprise_id: Option<String>,
    /// App ID.
    #[serde(default)]
    pub api_app_id: Option<String>,
    /// URL for replies after the ack. Valid for 30 minutes and 5 uses.
    #[serde(default)]
    pub response_url: String,
    /// ID to open a modal. Valid for 3 s.
    #[serde(default)]
    pub trigger_id: String,
}

// ----------------------------------------------------------------- replies

/// A message to post or to send as a reply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Message {
    /// Text. Slack uses it for notifications when blocks are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Block Kit blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<Value>>,
    /// Parent message timestamp. Posts in a thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    /// `ephemeral` or `in_channel`. Only for replies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_type: Option<String>,
    /// Replace the source message. Only for `response_url` replies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_original: Option<bool>,
    /// Delete the source message. Only for `response_url` replies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_original: Option<bool>,
}

impl Message {
    /// A message with this text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    /// Sets the blocks.
    #[must_use]
    pub fn blocks(mut self, blocks: Vec<Value>) -> Self {
        self.blocks = Some(blocks);
        self
    }

    /// Posts in the thread of `ts`.
    #[must_use]
    pub fn in_thread(mut self, ts: impl Into<String>) -> Self {
        self.thread_ts = Some(ts.into());
        self
    }

    /// Only the user sees the reply.
    #[must_use]
    pub fn ephemeral(mut self) -> Self {
        self.response_type = Some("ephemeral".to_owned());
        self
    }

    /// All channel members see the reply.
    #[must_use]
    pub fn in_channel(mut self) -> Self {
        self.response_type = Some("in_channel".to_owned());
        self
    }

    /// The reply replaces the source message.
    #[must_use]
    pub const fn replace_original(mut self) -> Self {
        self.replace_original = Some(true);
        self
    }

    /// The reply deletes the source message.
    #[must_use]
    pub const fn delete_original(mut self) -> Self {
        self.delete_original = Some(true);
        self
    }

    /// The message as JSON object fields.
    pub(crate) fn to_map(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

/// Reply to a slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandResponse {
    /// Empty 200. Slack shows nothing.
    Ack,
    /// Only the user sees the reply.
    Ephemeral(Message),
    /// All channel members see the reply.
    InChannel(Message),
}

impl CommandResponse {
    /// An ephemeral reply.
    #[must_use]
    pub fn ephemeral(msg: impl Into<Message>) -> Self {
        Self::Ephemeral(msg.into())
    }

    /// An in-channel reply.
    #[must_use]
    pub fn in_channel(msg: impl Into<Message>) -> Self {
        Self::InChannel(msg.into())
    }

    /// The reply message with `response_type` set. `None` for [`Self::Ack`].
    #[must_use]
    pub fn into_message(self) -> Option<Message> {
        match self {
            Self::Ack => None,
            Self::Ephemeral(m) => Some(m.ephemeral()),
            Self::InChannel(m) => Some(m.in_channel()),
        }
    }
}

// ------------------------------------------------------------ interactions

/// One action from a `block_actions` payload.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BlockAction {
    /// The action ID.
    pub action_id: String,
    /// The block ID.
    pub block_id: Option<String>,
    /// The `value` of a button.
    pub value: Option<String>,
    /// The action JSON.
    pub action: Value,
    /// User ID.
    pub user_id: Option<String>,
    /// Workspace ID.
    pub team_id: Option<String>,
    /// Channel ID, for a message action.
    pub channel_id: Option<String>,
    /// Timestamp of the message with the button.
    pub message_ts: Option<String>,
    /// URL for replies. Not set for actions in modals or the Home tab.
    pub response_url: Option<String>,
    /// ID to open a modal. Valid for 3 s.
    pub trigger_id: Option<String>,
    /// The full payload JSON.
    pub payload: Value,
}

/// A `view_submission` or `view_closed` payload.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ViewPayload {
    /// The view `callback_id`.
    pub callback_id: String,
    /// The view ID.
    pub view_id: Option<String>,
    /// The view `private_metadata`.
    pub private_metadata: Option<String>,
    /// The view JSON.
    pub view: Value,
    /// User ID.
    pub user_id: Option<String>,
    /// Workspace ID.
    pub team_id: Option<String>,
    /// ID to open a modal. Valid for 3 s.
    pub trigger_id: Option<String>,
    /// The full payload JSON.
    pub payload: Value,
}

impl ViewPayload {
    /// The input values: `view.state.values`.
    #[must_use]
    pub fn state_values(&self) -> Option<&Value> {
        self.view.get("state")?.get("values")
    }
}

/// Reply to a `view_submission`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ViewResponse {
    /// Close this view.
    Close,
    /// Close all views.
    Clear,
    /// Replace this view.
    Update(Value),
    /// Push a new view.
    Push(Value),
    /// Show errors by block ID. The view stays open.
    Errors(BTreeMap<String, String>),
}

impl ViewResponse {
    /// Errors by block ID.
    #[must_use]
    pub fn errors<K: Into<String>, V: Into<String>>(
        errors: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self::Errors(
            errors
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    /// The reply body. `None` for [`Self::Close`].
    #[must_use]
    pub fn to_body(&self) -> Option<Value> {
        let body = match self {
            Self::Close => return None,
            Self::Clear => serde_json::json!({"response_action": "clear"}),
            Self::Update(view) => serde_json::json!({"response_action": "update", "view": view}),
            Self::Push(view) => serde_json::json!({"response_action": "push", "view": view}),
            Self::Errors(errors) => {
                serde_json::json!({"response_action": "errors", "errors": errors})
            }
        };
        Some(body)
    }
}

/// Kind of shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutKind {
    /// A global shortcut (`shortcut`).
    Global,
    /// A message shortcut (`message_action`).
    Message,
}

/// A global or message shortcut.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Shortcut {
    /// Global or message.
    pub kind: ShortcutKind,
    /// The shortcut `callback_id`.
    pub callback_id: String,
    /// ID to open a modal. Valid for 3 s.
    pub trigger_id: String,
    /// User ID.
    pub user_id: Option<String>,
    /// Workspace ID.
    pub team_id: Option<String>,
    /// Channel ID, for a message shortcut.
    pub channel_id: Option<String>,
    /// The source message, for a message shortcut.
    pub message: Option<Value>,
    /// URL for replies, for a message shortcut.
    pub response_url: Option<String>,
    /// The full payload JSON.
    pub payload: Value,
}

/// Interaction payload, split by type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Interaction {
    BlockActions(Vec<BlockAction>),
    ViewSubmission(ViewPayload),
    ViewClosed(ViewPayload),
    Shortcut(Shortcut),
    /// `block_suggestion`: options load. Not in scope; the reply is no options.
    BlockSuggestion,
    /// A type with no support.
    Other,
}

impl Interaction {
    /// Splits an interaction payload. `None` when a needed field is missing.
    pub(crate) fn parse(payload: Value) -> Option<Self> {
        let kind = payload.get("type")?.as_str()?.to_owned();
        let user_id = str_at(&payload, &["user", "id"]);
        // In an org-wide install, `team` can be null. Then `user.team_id` has the ID.
        let team_id =
            str_at(&payload, &["team", "id"]).or_else(|| str_at(&payload, &["user", "team_id"]));
        let trigger_id = str_at(&payload, &["trigger_id"]);
        let response_url = str_at(&payload, &["response_url"]);
        let channel_id = str_at(&payload, &["channel", "id"]);
        match kind.as_str() {
            "block_actions" => {
                let actions = payload.get("actions")?.as_array()?;
                let message_ts = str_at(&payload, &["container", "message_ts"])
                    .or_else(|| str_at(&payload, &["message", "ts"]));
                let list = actions
                    .iter()
                    .map(|a| {
                        Some(BlockAction {
                            action_id: a.get("action_id")?.as_str()?.to_owned(),
                            block_id: str_at(a, &["block_id"]),
                            value: str_at(a, &["value"]),
                            action: a.clone(),
                            user_id: user_id.clone(),
                            team_id: team_id.clone(),
                            channel_id: channel_id.clone(),
                            message_ts: message_ts.clone(),
                            response_url: response_url.clone(),
                            trigger_id: trigger_id.clone(),
                            payload: payload.clone(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(Self::BlockActions(list))
            }
            "view_submission" | "view_closed" => {
                let view = payload.get("view")?.clone();
                let v = ViewPayload {
                    callback_id: str_at(&view, &["callback_id"])?,
                    view_id: str_at(&view, &["id"]),
                    private_metadata: str_at(&view, &["private_metadata"]),
                    view,
                    user_id,
                    team_id,
                    trigger_id,
                    payload,
                };
                Some(if kind == "view_submission" {
                    Self::ViewSubmission(v)
                } else {
                    Self::ViewClosed(v)
                })
            }
            "shortcut" | "message_action" => Some(Self::Shortcut(Shortcut {
                kind: if kind == "shortcut" {
                    ShortcutKind::Global
                } else {
                    ShortcutKind::Message
                },
                callback_id: str_at(&payload, &["callback_id"])?,
                trigger_id: trigger_id?,
                user_id,
                team_id,
                channel_id,
                message: payload.get("message").cloned(),
                response_url,
                payload,
            })),
            "block_suggestion" => Some(Self::BlockSuggestion),
            _ => Some(Self::Other),
        }
    }
}

/// The string at `path`, if any.
fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_serializes_only_set_fields() {
        let m = Message::text("hi").in_thread("1.0").ephemeral();
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"text": "hi", "thread_ts": "1.0", "response_type": "ephemeral"})
        );
        let m = Message::default().delete_original();
        assert_eq!(m.to_map().len(), 1);
        assert_eq!(Message::from(String::from("a")), Message::from("a"));
    }

    #[test]
    fn command_response_sets_type() {
        assert_eq!(CommandResponse::Ack.into_message(), None);
        let m = CommandResponse::in_channel("x").into_message().unwrap();
        assert_eq!(m.response_type.as_deref(), Some("in_channel"));
        let m = CommandResponse::ephemeral(Message::text("y"))
            .into_message()
            .unwrap();
        assert_eq!(m.response_type.as_deref(), Some("ephemeral"));
    }

    #[test]
    fn view_response_bodies() {
        assert_eq!(ViewResponse::Close.to_body(), None);
        assert_eq!(
            ViewResponse::Clear.to_body().unwrap(),
            json!({"response_action": "clear"})
        );
        assert_eq!(
            ViewResponse::Update(json!({"type": "modal"}))
                .to_body()
                .unwrap(),
            json!({"response_action": "update", "view": {"type": "modal"}})
        );
        assert_eq!(
            ViewResponse::errors([("b", "bad")]).to_body().unwrap(),
            json!({"response_action": "errors", "errors": {"b": "bad"}})
        );
    }

    #[test]
    fn event_callback_parse() {
        let ev: EventCallback = serde_json::from_value(json!({
            "event_id": "E1", "event": {"type": "message", "text": "t", "channel": "C"}
        }))
        .unwrap();
        assert_eq!(ev.event_type(), "message");
        let m: MessageEvent = ev.parse().unwrap();
        assert_eq!(m.text.as_deref(), Some("t"));
        let ev2: EventCallback =
            serde_json::from_value(json!({"event_id": "E2", "event": 5})).unwrap();
        assert_eq!(ev2.event_type(), "");
    }

    #[test]
    fn interaction_parse_kinds() {
        let p = json!({"type": "block_actions", "actions": [{"action_id": "a"}, {"action_id": "b"}], "user": {"id": "U"}});
        let Some(Interaction::BlockActions(acts)) = Interaction::parse(p) else {
            panic!("not block actions")
        };
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[1].action_id, "b");
        assert_eq!(acts[0].user_id.as_deref(), Some("U"));
        // Action in a view: no channel, no response_url, container has view.
        let p = json!({"type": "block_actions", "actions": [{"action_id": "a"}], "container": {"type": "view"}});
        let Some(Interaction::BlockActions(acts)) = Interaction::parse(p) else {
            panic!()
        };
        assert!(acts[0].response_url.is_none() && acts[0].message_ts.is_none());
        let p = json!({"type": "view_submission", "view": {"id": "V", "callback_id": "c", "private_metadata": "m"}});
        let Some(Interaction::ViewSubmission(v)) = Interaction::parse(p) else {
            panic!()
        };
        assert_eq!(
            (
                v.callback_id.as_str(),
                v.view_id.as_deref(),
                v.private_metadata.as_deref()
            ),
            ("c", Some("V"), Some("m"))
        );
        assert!(matches!(
            Interaction::parse(json!({"type": "view_closed", "view": {"callback_id": "c"}})),
            Some(Interaction::ViewClosed(_))
        ));
        assert!(matches!(
            Interaction::parse(json!({"type": "block_suggestion"})),
            Some(Interaction::BlockSuggestion)
        ));
        assert!(matches!(
            Interaction::parse(json!({"type": "new_kind"})),
            Some(Interaction::Other)
        ));
        // Org-wide install: `team` is null.
        let p = json!({"type": "view_closed", "team": null, "user": {"id": "U", "team_id": "T9"}, "view": {"callback_id": "c"}});
        let Some(Interaction::ViewClosed(v)) = Interaction::parse(p) else {
            panic!()
        };
        assert_eq!(v.team_id.as_deref(), Some("T9"));
        // Missing fields.
        assert_eq!(Interaction::parse(json!({"type": "view_submission"})), None);
        assert_eq!(Interaction::parse(json!({"type": "block_actions"})), None);
        assert_eq!(
            Interaction::parse(json!({"type": "shortcut", "callback_id": "x"})),
            None
        );
        assert_eq!(Interaction::parse(json!({"no": "type"})), None);
        assert_eq!(Interaction::parse(json!([1])), None);
    }

    #[test]
    fn slash_command_needs_command() {
        let c: SlashCommand = serde_urlencoded::from_str("command=%2Fa&text=b").unwrap();
        assert_eq!((c.command.as_str(), c.text.as_str()), ("/a", "b"));
        assert!(serde_urlencoded::from_str::<SlashCommand>("text=b").is_err());
    }
}
