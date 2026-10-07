//! `ReqwestTransport` against a local fake Slack server.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_plugin_slack::config::SlackConfig;
use autumn_plugin_slack::payload::Message;
use autumn_plugin_slack::transport::{HttpRequest, HttpTransport, ReqwestTransport};
use autumn_plugin_slack::{SlackError, SlackPlugin};
use autumn_web::AppState;
use autumn_web::reexports::axum::{
    self, Router,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use common::*;
use serde_json::json;

#[derive(Default, Clone)]
struct Seen {
    calls: Arc<Mutex<Vec<(HeaderMap, String)>>>,
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}/api/")
}

fn fake_slack(seen: &Seen) -> Router {
    let s1 = seen.clone();
    let s2 = seen.clone();
    Router::new()
        .route(
            "/api/chat.postMessage",
            post(move |headers: HeaderMap, body: String| {
                let s = s1.clone();
                async move {
                    let n = {
                        let mut calls = s.calls.lock().unwrap();
                        calls.push((headers, body));
                        calls.len()
                    };
                    if n == 1 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "1")],
                            "{\"ok\":false}",
                        )
                            .into_response()
                    } else {
                        axum::Json(json!({"ok": true, "channel": "C1", "ts": "7.7"}))
                            .into_response()
                    }
                }
            }),
        )
        .route(
            "/api/slow.method",
            post(move |headers: HeaderMap, body: String| {
                let s = s2.clone();
                async move {
                    s.calls.lock().unwrap().push((headers, body));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "{}"
                }
            }),
        )
}

fn cfg(base: &str) -> SlackConfig {
    let mut c = config();
    base.clone_into(&mut c.api_base_url);
    c.api.timeout_ms = 300;
    c
}

#[tokio::test]
async fn real_http_round_trip_with_429_retry() {
    let seen = Seen::default();
    let base = serve(fake_slack(&seen)).await;
    let rt = SlackPlugin::with_config(cfg(&base))
        .with_signing_secret(SECRET)
        .with_bot_token(TOKEN)
        .start(&AppState::for_test())
        .unwrap();
    let r = rt
        .client()
        .chat_post_message("C1", &Message::text("héllo & bye"))
        .await
        .unwrap();
    assert_eq!(r.ts, "7.7");
    let calls = seen.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    let (headers, body) = &calls[1];
    assert_eq!(headers["authorization"], "Bearer xoxb-test-token");
    let form: std::collections::BTreeMap<String, String> =
        serde_urlencoded::from_str(body).unwrap();
    assert_eq!(form["text"], "héllo & bye");
}

#[tokio::test]
async fn real_http_timeout_is_transport_error() {
    let seen = Seen::default();
    let base = serve(fake_slack(&seen)).await;
    let rt = SlackPlugin::with_config(cfg(&base))
        .with_signing_secret(SECRET)
        .with_bot_token(TOKEN)
        .start(&AppState::for_test())
        .unwrap();
    let e = rt
        .client()
        .call("slow.method", &json!({}))
        .await
        .unwrap_err();
    assert!(matches!(e, SlackError::Transport(_)), "{e:?}");
}

#[tokio::test]
async fn transport_reports_connect_failure_and_redacts_token() {
    // Port 9 (discard) on localhost: nothing listens.
    let t = ReqwestTransport::new(Duration::from_millis(500)).unwrap();
    let req = HttpRequest::new("http://127.0.0.1:9/api/x")
        .header("authorization", "Bearer xoxb-secret")
        .body(b"a=1".to_vec());
    assert!(
        !format!("{req:?}").contains("xoxb-secret"),
        "Debug leaks the token"
    );
    let e = t.post(req).await.unwrap_err();
    assert!(!e.to_string().contains("xoxb-secret"));
}
