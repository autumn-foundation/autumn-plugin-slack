//! Shared test helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use autumn_plugin_slack::SlackPlugin;
use autumn_plugin_slack::config::SlackConfig;
use autumn_plugin_slack::testing;
use autumn_plugin_slack::transport::MemoryTransport;
use autumn_web::reexports::chrono::{DateTime, TimeZone, Utc};
use autumn_web::test::{TestClient, TestResponse};
use autumn_web::time::FixedClock;

pub const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
pub const OLD_SECRET: &str = "old-secret-from-last-rotation-000";
pub const TOKEN: &str = "xoxb-test-token";
pub const RESPONSE_URL: &str = "https://hooks.slack.com/commands/T1/123/abc";

/// Fixed test time: 2026-01-01T00:00:00Z.
pub fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

pub fn now_secs() -> u64 {
    u64::try_from(now().timestamp()).unwrap()
}

/// Test config. The ack window is wide, so in-time tests do not depend on
/// CPU load. Slow-path tests use [`slow_config`] and a [`Gate`].
pub fn config() -> SlackConfig {
    let mut c = SlackConfig::default();
    c.ack_timeout_ms = 2_000;
    c.api.initial_backoff_ms = 1;
    c
}

/// Config whose bot token env var is never set, so no token is found.
pub fn no_token_config() -> SlackConfig {
    let mut c = config();
    "AUTUMN_SLACK_TEST_NO_SUCH_TOKEN".clone_into(&mut c.bot_token_env);
    c
}

/// Config with a short ack window, for slow-path tests.
pub fn slow_config() -> SlackConfig {
    let mut c = config();
    c.ack_timeout_ms = 50;
    c
}

/// A closed gate. A handler waits on it; the test opens it. No timing.
#[derive(Clone, Default)]
pub struct Gate(Arc<tokio::sync::Notify>);

impl Gate {
    pub async fn wait(&self) {
        self.0.notified().await;
    }

    /// Opens the gate. A later `wait` also passes.
    pub fn open(&self) {
        self.0.notify_one();
    }
}

/// Fails if `fut` does not end in 10 s. Use it where a hang is the bug.
pub async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("did not end in 10 s")
}

/// A plugin with test secrets, a fixed clock, and this fake transport.
pub fn plugin(t: &MemoryTransport) -> SlackPlugin {
    plugin_with(t, config())
}

pub fn plugin_with(t: &MemoryTransport, c: SlackConfig) -> SlackPlugin {
    SlackPlugin::with_config(c)
        .with_signing_secret(SECRET)
        .with_bot_token(TOKEN)
        .with_transport(t.clone())
        .with_clock(Arc::new(FixedClock::at(now())))
}

/// Sends a signed POST with the current test time.
pub async fn post_signed(
    client: &TestClient,
    path: &str,
    content_type: &str,
    body: &str,
) -> TestResponse {
    post_signed_at(client, path, content_type, body, now_secs(), SECRET).await
}

pub async fn post_signed_at(
    client: &TestClient,
    path: &str,
    content_type: &str,
    body: &str,
    ts: u64,
    secret: &str,
) -> TestResponse {
    let sig = testing::sign(secret, ts, body.as_bytes());
    client
        .post(path)
        .header("content-type", content_type)
        .header("x-slack-request-timestamp", &ts.to_string())
        .header("x-slack-signature", &sig)
        .body(body.to_owned())
        .send()
        .await
}

pub async fn post_event(client: &TestClient, body: &serde_json::Value) -> TestResponse {
    post_signed(
        client,
        "/slack/events",
        "application/json",
        &body.to_string(),
    )
    .await
}

pub async fn post_form(client: &TestClient, path: &str, pairs: &[(&str, &str)]) -> TestResponse {
    let body = serde_urlencoded::to_string(pairs).unwrap();
    post_signed(client, path, "application/x-www-form-urlencoded", &body).await
}

pub async fn post_interaction(client: &TestClient, payload: &serde_json::Value) -> TestResponse {
    post_form(
        client,
        "/slack/interactions",
        &[("payload", &payload.to_string())],
    )
    .await
}

pub fn command_form<'a>(command: &'a str, text: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("token", "legacy"),
        ("team_id", "T1"),
        ("team_domain", "acme"),
        ("channel_id", "C1"),
        ("channel_name", "general"),
        ("user_id", "U1"),
        ("user_name", "ada"),
        ("command", command),
        ("text", text),
        ("api_app_id", "A1"),
        ("response_url", RESPONSE_URL),
        ("trigger_id", "trig.1"),
    ]
}

pub fn event_callback(event_id: &str, event: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "token": "legacy",
        "team_id": "T1",
        "api_app_id": "A1",
        "type": "event_callback",
        "event_id": event_id,
        "event_time": 1_767_225_600,
        "event": event,
    })
}

/// Waits for one value, or fails after 5 s.
pub async fn recv<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out")
        .expect("channel closed")
}

/// Waits until `f` is true, or fails after `limit`.
pub async fn wait_until(limit: Duration, f: &(impl Fn() -> bool + Sync)) {
    let start = tokio::time::Instant::now();
    while !f() {
        assert!(start.elapsed() < limit, "condition not met in {limit:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
