//! Slack Web API client and `response_url` replies.

use std::sync::Arc;

use autumn_web::AppState;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config::SlackConfig;
use crate::error::SlackError;
use crate::metrics::SlackMetrics;
use crate::payload::Message;
use crate::policy::{Outcome, RetryRule, Step, decide, retry_after_ms};
use crate::transport::{HttpReply, HttpRequest, HttpTransport, TransportError};

/// Largest 429 wait for a call with a `trigger_id`. A trigger is valid for
/// 3 s only, so a longer wait gives `expired_trigger_id`.
const TRIGGER_MAX_WAIT_MS: u64 = 1_000;

/// Metrics label for `response_url` posts.
const RESPONSE_URL_LABEL: &str = "response_url";
/// Fields that only a reply can have. API calls drop them.
const REPLY_ONLY: [&str; 3] = ["response_type", "replace_original", "delete_original"];

/// Where a message is: channel and timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct MessageRef {
    /// Channel ID.
    pub channel: String,
    /// Message timestamp (its ID).
    pub ts: String,
}

/// Result of `auth.test`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct AuthTest {
    /// Workspace ID.
    pub team_id: String,
    /// Bot user ID.
    pub user_id: String,
    /// Bot ID.
    #[serde(default)]
    pub bot_id: Option<String>,
    /// Workspace name.
    #[serde(default)]
    pub team: Option<String>,
    /// Workspace URL.
    #[serde(default)]
    pub url: Option<String>,
}

struct Inner {
    transport: Arc<dyn HttpTransport>,
    config: SlackConfig,
    token: Option<String>,
    metrics: Arc<SlackMetrics>,
}

/// Slack Web API client. A clone shares one connection pool.
///
/// Get it from a handler with [`crate::SlackContext::client`], or from the
/// app state with [`SlackClient::from_state`].
#[derive(Clone)]
pub struct SlackClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SlackClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackClient")
            .field("api_base_url", &self.inner.config.api_base_url)
            .field("has_token", &self.inner.token.is_some())
            .finish_non_exhaustive()
    }
}

impl SlackClient {
    pub(crate) fn new(
        transport: Arc<dyn HttpTransport>,
        config: SlackConfig,
        token: Option<String>,
        metrics: Arc<SlackMetrics>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                transport,
                config,
                token,
                metrics,
            }),
        }
    }

    /// Returns the client that the plugin put in the app state at startup.
    #[must_use]
    pub fn from_state(state: &AppState) -> Option<Self> {
        state.extension::<Self>().map(|c| (*c).clone())
    }

    /// Returns `true` when a bot token is set.
    #[must_use]
    pub fn has_token(&self) -> bool {
        self.inner.token.is_some()
    }

    /// Calls a write method. It retries only on 429.
    ///
    /// `params` is a JSON object. The client sends strings unchanged and other
    /// values as JSON text. It does not send `null` values.
    ///
    /// # Errors
    /// Returns [`SlackError::Api`] for `ok: false`, or a transport, HTTP,
    /// rate-limit, decode, or config error.
    pub async fn call(&self, method: &str, params: &Value) -> Result<Value, SlackError> {
        self.call_with(method, params, false, self.rule()).await
    }

    /// Calls a read method. It also retries on 5xx and network errors.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn call_read(&self, method: &str, params: &Value) -> Result<Value, SlackError> {
        self.call_with(method, params, true, self.rule()).await
    }

    /// The retry rule from the config.
    fn rule(&self) -> RetryRule {
        self.inner.config.api.rule()
    }

    /// The rule for calls with a `trigger_id`: a short wait cap.
    fn trigger_rule(&self) -> RetryRule {
        let mut r = self.rule();
        r.max_wait_ms = r.max_wait_ms.min(TRIGGER_MAX_WAIT_MS);
        r
    }

    async fn call_with(
        &self,
        method: &str,
        params: &Value,
        idempotent: bool,
        rule: RetryRule,
    ) -> Result<Value, SlackError> {
        if !valid_method(method) {
            return Err(SlackError::Config(format!(
                "not a Web API method name: {method:?}"
            )));
        }
        let Value::Object(map) = params else {
            return Err(SlackError::Config(format!(
                "{method}: params must be a JSON object"
            )));
        };
        let Some(token) = self.inner.token.as_deref() else {
            return Err(SlackError::Config(format!(
                "{method}: no bot token; set the env var {}",
                self.inner.config.bot_token_env
            )));
        };
        let url = format!("{}{method}", self.inner.config.api_base_url);
        let body = form_body(map).into_bytes();
        let mut attempt: u32 = 1;
        loop {
            let req = HttpRequest::new(url.as_str())
                .header("authorization", format!("Bearer {token}"))
                .header(
                    "content-type",
                    "application/x-www-form-urlencoded; charset=utf-8",
                )
                .body(body.clone());
            let result = self.inner.transport.post(req).await;
            let (outcome, retry_after) = classify(&result);
            match decide(
                outcome,
                attempt,
                idempotent,
                retry_after.map(retry_after_ms),
                &rule,
            ) {
                Step::Retry(wait_ms) => {
                    self.inner.metrics.api_retry(method);
                    tracing::debug!(method, attempt, wait_ms, "slack api retry");
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    attempt += 1;
                }
                Step::Done | Step::GiveUp => return self.finish(method, result),
            }
        }
    }

    /// Turns the last attempt into a result. Counts it.
    fn finish(
        &self,
        method: &str,
        result: Result<HttpReply, TransportError>,
    ) -> Result<Value, SlackError> {
        let m = &self.inner.metrics;
        let reply = match result {
            Ok(r) => r,
            Err(e) => {
                m.api_call(method, "transport");
                return Err(SlackError::Transport(e.0));
            }
        };
        if reply.status == 429 {
            m.api_call(method, "rate_limited");
            return Err(SlackError::RateLimited {
                retry_after_secs: reply.retry_after_secs,
            });
        }
        if !(200..300).contains(&reply.status) {
            m.api_call(method, "http_error");
            return Err(http_error(&reply));
        }
        let v: Value = serde_json::from_slice(&reply.body).map_err(|e| {
            m.api_call(method, "decode");
            SlackError::Decode(format!("{method}: {e}"))
        })?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            m.api_call(method, "ok");
            return Ok(v);
        }
        m.api_call(method, "api_error");
        Err(SlackError::Api {
            method: method.to_owned(),
            code: v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_owned(),
            needed: v.get("needed").and_then(Value::as_str).map(str::to_owned),
        })
    }

    /// Calls `method` and decodes the reply as `T`.
    async fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        idempotent: bool,
    ) -> Result<T, SlackError> {
        let v = self
            .call_with(method, &params, idempotent, self.rule())
            .await?;
        T::deserialize(&v).map_err(|e| SlackError::Decode(format!("{method}: {e}")))
    }

    /// `chat.postMessage`.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn chat_post_message(
        &self,
        channel: &str,
        msg: &Message,
    ) -> Result<MessageRef, SlackError> {
        let mut p = api_fields(msg);
        p.insert("channel".to_owned(), channel.into());
        self.call_as("chat.postMessage", Value::Object(p), false)
            .await
    }

    /// `chat.postEphemeral`. Returns the `message_ts`.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn chat_post_ephemeral(
        &self,
        channel: &str,
        user: &str,
        msg: &Message,
    ) -> Result<String, SlackError> {
        #[derive(Deserialize)]
        struct Reply {
            message_ts: String,
        }
        let mut p = api_fields(msg);
        p.insert("channel".to_owned(), channel.into());
        p.insert("user".to_owned(), user.into());
        let r: Reply = self
            .call_as("chat.postEphemeral", Value::Object(p), false)
            .await?;
        Ok(r.message_ts)
    }

    /// `chat.update`.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn chat_update(
        &self,
        channel: &str,
        ts: &str,
        msg: &Message,
    ) -> Result<MessageRef, SlackError> {
        let mut p = api_fields(msg);
        // `chat.update` cannot move a message to a thread.
        p.remove("thread_ts");
        p.insert("channel".to_owned(), channel.into());
        p.insert("ts".to_owned(), ts.into());
        self.call_as("chat.update", Value::Object(p), false).await
    }

    /// `chat.delete`.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn chat_delete(&self, channel: &str, ts: &str) -> Result<(), SlackError> {
        self.call(
            "chat.delete",
            &serde_json::json!({"channel": channel, "ts": ts}),
        )
        .await
        .map(drop)
    }

    /// `reactions.add`. `name` has no colons, for example `thumbsup`.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn reactions_add(
        &self,
        channel: &str,
        ts: &str,
        name: &str,
    ) -> Result<(), SlackError> {
        let p = serde_json::json!({"channel": channel, "timestamp": ts, "name": name.trim_matches(':')});
        self.call("reactions.add", &p).await.map(drop)
    }

    /// `views.open`. Returns the view.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn views_open(&self, trigger_id: &str, view: &Value) -> Result<Value, SlackError> {
        let p = serde_json::json!({"trigger_id": trigger_id, "view": view});
        self.view_call("views.open", p, self.trigger_rule()).await
    }

    /// `views.update`. `hash` stops an update that would overwrite a newer
    /// view. Returns the view.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn views_update(
        &self,
        view_id: &str,
        view: &Value,
        hash: Option<&str>,
    ) -> Result<Value, SlackError> {
        let p = serde_json::json!({"view_id": view_id, "view": view, "hash": hash});
        self.view_call("views.update", p, self.rule()).await
    }

    /// `views.push`. Returns the view.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn views_push(&self, trigger_id: &str, view: &Value) -> Result<Value, SlackError> {
        let p = serde_json::json!({"trigger_id": trigger_id, "view": view});
        self.view_call("views.push", p, self.trigger_rule()).await
    }

    /// `views.publish` for the Home tab. Returns the view.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn views_publish(&self, user_id: &str, view: &Value) -> Result<Value, SlackError> {
        let p = serde_json::json!({"user_id": user_id, "view": view});
        self.view_call("views.publish", p, self.rule()).await
    }

    /// Calls a `views.*` write method. Returns the `view` field.
    async fn view_call(
        &self,
        method: &str,
        params: Value,
        rule: RetryRule,
    ) -> Result<Value, SlackError> {
        let mut v = self.call_with(method, &params, false, rule).await?;
        Ok(v.get_mut("view").map(Value::take).unwrap_or_default())
    }

    /// `users.info`. Returns the user.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn users_info(&self, user: &str) -> Result<Value, SlackError> {
        let mut v = self
            .call_read("users.info", &serde_json::json!({"user": user}))
            .await?;
        Ok(v.get_mut("user").map(Value::take).unwrap_or_default())
    }

    /// `auth.test`.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn auth_test(&self) -> Result<AuthTest, SlackError> {
        self.call_as("auth.test", Value::Object(Map::new()), true)
            .await
    }

    /// `auth.test` with one attempt, for the health check. A 429 gives
    /// `RateLimited` at once, not a long wait.
    pub(crate) async fn auth_test_once(&self) -> Result<(), SlackError> {
        let mut rule = self.rule();
        rule.max_attempts = 1;
        self.call_with("auth.test", &Value::Object(Map::new()), true, rule)
            .await
            .map(drop)
    }

    /// Calls a read method once per page and joins the `key` arrays.
    /// It follows `response_metadata.next_cursor` for at most `max_pages`.
    ///
    /// # Errors
    /// Same as [`Self::call`].
    pub async fn paginate(
        &self,
        method: &str,
        params: &Value,
        key: &str,
        max_pages: usize,
    ) -> Result<Vec<Value>, SlackError> {
        let Value::Object(base) = params else {
            return Err(SlackError::Config(format!(
                "{method}: params must be a JSON object"
            )));
        };
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages {
            let mut p = base.clone();
            if let Some(c) = cursor.take() {
                p.insert("cursor".to_owned(), Value::String(c));
            }
            let mut v = self.call_read(method, &Value::Object(p)).await?;
            if let Some(Value::Array(items)) = v.get_mut(key).map(Value::take) {
                out.extend(items);
            }
            cursor = v
                .pointer("/response_metadata/next_cursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Posts `msg` as JSON to a `response_url`. It sends no token.
    ///
    /// # Errors
    /// Returns [`SlackError::BadResponseUrl`] for a URL that is not HTTPS on
    /// an allowed host. Returns an HTTP or transport error.
    pub async fn respond(&self, response_url: &str, msg: &Message) -> Result<(), SlackError> {
        let m = &self.inner.metrics;
        if !allowed_response_url(response_url, &self.inner.config.response_url_hosts) {
            m.api_call(RESPONSE_URL_LABEL, "rejected");
            return Err(SlackError::BadResponseUrl);
        }
        let body = serde_json::to_vec(msg).map_err(|e| SlackError::Decode(e.to_string()))?;
        let req = HttpRequest::new(response_url)
            .header("content-type", "application/json; charset=utf-8")
            .body(body);
        // One attempt: a `response_url` allows only 5 uses.
        match self.inner.transport.post(req).await {
            Err(e) => {
                m.api_call(RESPONSE_URL_LABEL, "transport");
                Err(SlackError::Transport(e.0))
            }
            Ok(r) if (200..300).contains(&r.status) => {
                m.api_call(RESPONSE_URL_LABEL, "ok");
                Ok(())
            }
            Ok(r) => {
                m.api_call(RESPONSE_URL_LABEL, "http_error");
                Err(http_error(&r))
            }
        }
    }
}

/// Classifies one attempt for the retry policy.
fn classify(result: &Result<HttpReply, TransportError>) -> (Outcome, Option<u64>) {
    result
        .as_ref()
        .map_or((Outcome::NetworkError, None), |r| match r.status {
            429 => (Outcome::RateLimited, r.retry_after_secs),
            200..=299 => (Outcome::Success, None),
            500..=599 => (Outcome::ServerError, None),
            _ => (Outcome::ClientError, None),
        })
}

/// An HTTP error with the first 200 bytes of the body.
fn http_error(reply: &HttpReply) -> SlackError {
    let end = reply.body.len().min(200);
    SlackError::Http {
        status: reply.status,
        body: String::from_utf8_lossy(&reply.body[..end]).into_owned(),
    }
}

/// The message fields for an API call. Reply-only fields are dropped.
fn api_fields(msg: &Message) -> Map<String, Value> {
    let mut p = msg.to_map();
    for k in REPLY_ONLY {
        p.remove(k);
    }
    p
}

/// Returns `true` for an `https` URL on an allowed host, with no user info
/// and no port.
pub(crate) fn allowed_response_url(url: &str, hosts: &[String]) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    u.scheme() == "https"
        && u.username().is_empty()
        && u.password().is_none()
        && u.port().is_none()
        && u.host_str()
            .is_some_and(|h| hosts.iter().any(|a| a.eq_ignore_ascii_case(h)))
}

/// Returns `true` for a method name such as `chat.postMessage`.
pub(crate) fn valid_method(method: &str) -> bool {
    let mut parts = 0;
    for part in method.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// Encodes JSON object params as a form body.
pub(crate) fn form_body(params: &Map<String, Value>) -> String {
    let pairs: Vec<(&str, String)> = params
        .iter()
        .filter_map(|(k, v)| {
            let text = match v {
                Value::Null => return None,
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some((k.as_str(), text))
        })
        .collect();
    // String pairs cannot fail to encode.
    serde_urlencoded::to_string(pairs).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn hosts() -> Vec<String> {
        vec!["hooks.slack.com".to_owned()]
    }

    #[test]
    fn response_url_rules() {
        assert!(allowed_response_url(
            "https://hooks.slack.com/commands/T/1/x",
            &hosts()
        ));
        assert!(allowed_response_url(
            "https://HOOKS.slack.com/actions/T/1/x",
            &hosts()
        ));
        for bad in [
            "http://hooks.slack.com/x",
            "https://hooks.slack.com.evil/x",
            "https://evil/hooks.slack.com/x",
            "https://a@hooks.slack.com/x",
            "https://hooks.slack.com:444/x",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!allowed_response_url(bad, &hosts()), "{bad}");
        }
    }

    #[test]
    fn method_names() {
        assert!(valid_method("chat.postMessage"));
        assert!(valid_method("admin.conversations.setTeams"));
        for bad in [
            "chat", "chat.", ".x", "a..b", "../x", "a/b", "a.b?c", "a.b c", "",
        ] {
            assert!(!valid_method(bad), "{bad}");
        }
    }

    #[test]
    fn form_encoding() {
        let p = json!({"a": "x y&z", "n": 1, "b": false, "o": {"k": [1]}, "z": null});
        let body = form_body(p.as_object().unwrap());
        let back: std::collections::BTreeMap<String, String> =
            serde_urlencoded::from_str(&body).unwrap();
        assert_eq!(back["a"], "x y&z");
        assert_eq!(back["n"], "1");
        assert_eq!(back["b"], "false");
        assert_eq!(back["o"], r#"{"k":[1]}"#);
        assert!(!back.contains_key("z"));
    }

    proptest! {
        #[test]
        fn form_round_trips_strings(k in "[a-z_]{1,10}", v in ".{0,40}") {
            let mut m = Map::new();
            m.insert(k.clone(), Value::String(v.clone()));
            let back: std::collections::BTreeMap<String, String> = serde_urlencoded::from_str(&form_body(&m)).unwrap();
            prop_assert_eq!(&back[&k], &v);
        }

        #[test]
        fn only_listed_hosts_pass(host in "[a-z]{1,12}(\\.[a-z]{2,6}){1,2}", path in "[a-z/]{0,20}") {
            let url = format!("https://{host}/{path}");
            prop_assert_eq!(allowed_response_url(&url, &hosts()), host == "hooks.slack.com");
        }
    }
}
