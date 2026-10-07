//! Request processing: verify, parse, dedup, dispatch, ack.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use autumn_web::AppState;
use autumn_web::reexports::axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use autumn_web::time::ClockSource;
use autumn_web::webhook::WebhookReplayStore;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::client::SlackClient;
use crate::config::SlackConfig;
use crate::handlers::{Handler, Registry, RunOutcome, Runner, SlackContext};
use crate::metrics::{SlackMetrics, Surface};
use crate::payload::{CommandResponse, EventCallback, Interaction, Message, SlashCommand};
use crate::routes::{json as json_reply, status};
use crate::verify::{SIGNATURE_HEADER, TIMESTAMP_HEADER, Verifier};

/// Header with the retry count of an Events API delivery.
const RETRY_NUM_HEADER: &str = "x-slack-retry-num";
/// Header with the retry reason of an Events API delivery.
const RETRY_REASON_HEADER: &str = "x-slack-retry-reason";

/// The started plugin core. Routes call it.
pub(crate) struct Engine {
    pub config: SlackConfig,
    pub verifier: Verifier,
    pub registry: Registry,
    pub client: SlackClient,
    pub metrics: Arc<SlackMetrics>,
    pub clock: Arc<dyn ClockSource>,
    pub replay: Arc<dyn WebhookReplayStore>,
    pub runner: Runner,
    pub state: AppState,
}

fn ok() -> Response {
    status(StatusCode::OK)
}

fn bad_request() -> Response {
    status(StatusCode::BAD_REQUEST)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

impl Engine {
    fn ctx(&self) -> SlackContext {
        SlackContext::new(self.client.clone(), self.state.clone())
    }

    fn now_secs(&self) -> u64 {
        u64::try_from(self.clock.now().timestamp()).unwrap_or(0)
    }

    const fn ack_timeout(&self) -> Duration {
        Duration::from_millis(self.config.ack_timeout_ms)
    }

    /// Checks the signature. On failure, returns the reply to send.
    fn check(&self, surface: Surface, headers: &HeaderMap, body: &[u8]) -> Option<Response> {
        self.verifier
            .verify(
                header(headers, TIMESTAMP_HEADER),
                header(headers, SIGNATURE_HEADER),
                body,
                self.now_secs(),
            )
            .map_err(|e| {
                self.metrics.request(surface, e.label());
                tracing::debug!(surface = surface.as_str(), error = %e, "slack request rejected");
                let code = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::UNAUTHORIZED);
                status(code)
            })
            .err()
    }

    fn reject(&self, surface: Surface) -> Response {
        self.metrics.request(surface, "bad_request");
        bad_request()
    }

    fn unhandled(&self, surface: Surface) -> Response {
        self.metrics.request(surface, "unhandled");
        ok()
    }

    fn error_message(&self) -> Message {
        Message::text(self.config.error_text.clone()).ephemeral()
    }

    /// Handles `POST {base}/events`.
    pub(crate) async fn events(&self, headers: &HeaderMap, body: Bytes) -> Response {
        const S: Surface = Surface::Events;
        if let Some(r) = self.check(S, headers, &body) {
            return r;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&body) else {
            return self.reject(S);
        };
        match v.get("type").and_then(Value::as_str) {
            Some("url_verification") => {
                let Some(challenge) = v.get("challenge").and_then(Value::as_str) else {
                    return self.reject(S);
                };
                self.metrics.request(S, "ok");
                json_reply(&json!({ "challenge": challenge }))
            }
            Some("event_callback") => {
                let Ok(mut ev) = serde_json::from_value::<EventCallback>(v) else {
                    return self.reject(S);
                };
                ev.retry_num =
                    header(headers, RETRY_NUM_HEADER).and_then(|n| n.trim().parse().ok());
                ev.retry_reason = header(headers, RETRY_REASON_HEADER).map(str::to_owned);
                self.event_callback(ev).await
            }
            Some("app_rate_limited") => {
                tracing::warn!("slack app_rate_limited: Slack drops events for one minute");
                self.unhandled(S)
            }
            _ => self.unhandled(S),
        }
    }

    async fn event_callback(&self, ev: EventCallback) -> Response {
        const S: Surface = Surface::Events;
        let Some(handler) = self.registry.events.get(ev.event_type()) else {
            return self.unhandled(S);
        };
        if self.seen_before(&ev.event_id).await {
            // 200, so Slack stops the retries.
            self.metrics.request(S, "duplicate");
            return ok();
        }
        self.metrics.request(S, "ok");
        drop(self.runner.spawn(S, Arc::clone(handler), self.ctx(), ev));
        ok()
    }

    /// Returns `true` for an event ID inside the dedup window. A store error
    /// gives `false`: delivery is at least once.
    async fn seen_before(&self, event_id: &str) -> bool {
        let key = format!("slack:event:{event_id}");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(self.now_secs());
        let window = Duration::from_secs(self.config.dedup_window_secs);
        match self.replay.check_and_insert(&key, now, window).await {
            Ok(fresh) => !fresh,
            Err(e) => {
                tracing::warn!(error = %e, "slack dedup store failed; running the handler");
                false
            }
        }
    }

    /// Handles `POST {base}/commands`.
    pub(crate) async fn commands(&self, headers: &HeaderMap, body: Bytes) -> Response {
        const S: Surface = Surface::Commands;
        // Slack can send `ssl_check` with no signature. The reply is a fixed
        // empty 200 and no handler runs, so it is safe before the check. The
        // scan keeps no pairs, so an unsigned body costs no extra memory.
        if url::form_urlencoded::parse(&body).any(|(k, v)| k == "ssl_check" && v == "1") {
            self.metrics.request(S, "ssl_check");
            return ok();
        }
        if let Some(r) = self.check(S, headers, &body) {
            return r;
        }
        let Ok(cmd) = serde_urlencoded::from_bytes::<SlashCommand>(&body) else {
            return self.reject(S);
        };
        let Some(handler) = self.registry.commands.get(&cmd.command) else {
            self.metrics.request(S, "unhandled");
            let msg = Message::text(self.config.unknown_command_text.clone()).ephemeral();
            return message_reply(&msg);
        };
        self.metrics.request(S, "ok");
        let response_url = cmd.response_url.clone();
        let client = self.client.clone();
        let error = self.error_message();
        let rx = self.ack_or_late(S, Arc::clone(handler), cmd, move |res| async move {
            // Too slow: the ack went out empty. Reply to `response_url`.
            let msg = res.map_or(Some(error), CommandResponse::into_message);
            if let Some(msg) = msg {
                late_reply(&client, &response_url, &msg).await;
            }
        });
        match rx.await {
            Ok(Some(resp)) => resp.into_message().map_or_else(ok, |m| message_reply(&m)),
            Ok(None) => message_reply(&self.error_message()),
            Err(_) => ok(),
        }
    }

    /// Runs `handler` with an ack window. One tracked task owns the wait.
    ///
    /// The returned receiver gets the result when the handler ends in time
    /// (`None` for an error or a panic). Otherwise the sender drops, and the
    /// task calls `late` with the result when the handler ends. It also calls
    /// `late` when the request is gone before the result.
    fn ack_or_late<T, R, F, Fut>(
        &self,
        surface: Surface,
        handler: Handler<T, R>,
        payload: T,
        late: F,
    ) -> oneshot::Receiver<Option<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: FnOnce(Option<R>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let mut handle = self.runner.spawn(surface, handler, self.ctx(), payload);
        let ack = self.ack_timeout();
        let metrics = Arc::clone(&self.metrics);
        self.runner.spawn_task(async move {
            let res = if let Ok(done) = tokio::time::timeout(ack, &mut handle).await {
                match tx.send(finished_ok(done)) {
                    Ok(()) => return,
                    // The request is gone: the ack is not late. Use the late path.
                    Err(res) => res,
                }
            } else {
                // Count now: a handler that never ends must still show.
                metrics.late_ack(surface);
                drop(tx);
                finished_ok(handle.await)
            };
            late(res).await;
        });
        rx
    }

    /// Handles `POST {base}/interactions`.
    pub(crate) async fn interactions(&self, headers: &HeaderMap, body: Bytes) -> Response {
        const S: Surface = Surface::Interactions;
        if let Some(r) = self.check(S, headers, &body) {
            return r;
        }
        let form: BTreeMap<String, String> =
            serde_urlencoded::from_bytes(&body).unwrap_or_default();
        let Some(interaction) = form
            .get("payload")
            .and_then(|p| serde_json::from_str::<Value>(p).ok())
            .and_then(Interaction::parse)
        else {
            return self.reject(S);
        };
        match interaction {
            Interaction::BlockActions(actions) => {
                let mut any_handler = false;
                for action in actions {
                    let Some(handler) = self.registry.actions.get(&action.action_id) else {
                        continue;
                    };
                    any_handler = true;
                    let url = action.response_url.clone();
                    let handle = self
                        .runner
                        .spawn(S, Arc::clone(handler), self.ctx(), action);
                    let client = self.client.clone();
                    self.runner.spawn_task(async move {
                        let Some(Some(msg)) = finished_ok(handle.await) else {
                            return;
                        };
                        if let Some(url) = url {
                            late_reply(&client, &url, &msg).await;
                        } else {
                            tracing::warn!("slack action reply has no response_url; dropped");
                        }
                    });
                }
                if any_handler {
                    self.metrics.request(S, "ok");
                    ok()
                } else {
                    self.unhandled(S)
                }
            }
            Interaction::ViewSubmission(view) => {
                let Some(handler) = self.registry.view_submissions.get(&view.callback_id) else {
                    return self.unhandled(S);
                };
                self.metrics.request(S, "ok");
                let rx = self.ack_or_late(S, Arc::clone(handler), view, |_| async {
                    tracing::warn!("slack view_submission reply lost: the ack went out first");
                });
                // An empty 200 closes the view. No error detail goes to Slack.
                rx.await
                    .ok()
                    .flatten()
                    .and_then(|r| r.to_body())
                    .map_or_else(ok, |b| json_reply(&b))
            }
            Interaction::ViewClosed(view) => {
                let Some(handler) = self.registry.view_closed.get(&view.callback_id) else {
                    return self.unhandled(S);
                };
                self.metrics.request(S, "ok");
                drop(self.runner.spawn(S, Arc::clone(handler), self.ctx(), view));
                ok()
            }
            Interaction::Shortcut(sc) => {
                let Some(handler) = self.registry.shortcuts.get(&sc.callback_id) else {
                    return self.unhandled(S);
                };
                self.metrics.request(S, "ok");
                drop(self.runner.spawn(S, Arc::clone(handler), self.ctx(), sc));
                ok()
            }
            Interaction::BlockSuggestion => {
                // Options load is not in scope. No options shows no error in Slack.
                self.metrics.request(S, "unhandled");
                json_reply(&json!({ "options": [] }))
            }
            Interaction::Other => self.unhandled(S),
        }
    }
}

/// The handler value, or `None` for an error, a panic, or a cancelled task.
fn finished_ok<R>(joined: Result<RunOutcome<R>, tokio::task::JoinError>) -> Option<R> {
    joined.ok().and_then(RunOutcome::into_ok)
}

/// A 200 with the message as JSON.
fn message_reply(msg: &Message) -> Response {
    json_reply(&Value::Object(msg.to_map()))
}

/// Posts a late reply. Logs a failure.
async fn late_reply(client: &SlackClient, url: &str, msg: &Message) {
    if let Err(e) = client.respond(url, msg).await {
        tracing::warn!(error = %e, "slack late reply failed");
    }
}
