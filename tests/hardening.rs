//! Regression tests from the multi-angle code review.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use autumn_plugin_slack::payload::{CommandResponse, EventCallback, Message};
use autumn_plugin_slack::transport::{HttpReply, MemoryTransport};
use autumn_plugin_slack::{SlackError, SlackPlugin, SlackRuntime, testing};
use autumn_web::actuator::{HealthIndicator, HealthStatus};
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{AppState, ProcessRole};
use common::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower::ServiceExt as _;

fn state_with(cfg: AutumnConfig) -> AppState {
    let s = AppState::for_test();
    s.insert_extension(cfg);
    s
}

/// Sends a request to the runtime router. Returns the status.
async fn send(rt: &SlackRuntime, path: &str, body: &str, secret: Option<&str>) -> u16 {
    use autumn_web::reexports::axum::body::Body;
    use autumn_web::reexports::http::Request;
    let mut req = Request::post(path);
    if let Some(secret) = secret {
        for (k, v) in testing::signed_headers(secret, now_secs(), body.as_bytes()) {
            req = req.header(k, v);
        }
    }
    let req = req.body(Body::from(body.to_owned())).unwrap();
    rt.router().oneshot(req).await.unwrap().status().as_u16()
}

// ------------------------------------------------------- security review

#[tokio::test(flavor = "multi_thread")]
async fn every_route_rejects_bad_signatures_before_any_handler() {
    let t = MemoryTransport::new();
    let p = plugin(&t)
        .on_event("message", |_c, _e| async { panic!("event handler ran") })
        .command("/x", |_c, _c2| async { panic!("command handler ran") })
        .action("a", |_c, _a| async { panic!("action handler ran") });
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    let event = event_callback("EvSig", &json!({"type": "message"})).to_string();
    let command = serde_urlencoded::to_string(command_form("/x", "")).unwrap();
    let action = serde_urlencoded::to_string([(
        "payload",
        json!({"type": "block_actions", "actions": [{"action_id": "a"}]}).to_string(),
    )])
    .unwrap();
    for (path, body) in [
        ("/slack/events", event.as_str()),
        ("/slack/commands", command.as_str()),
        ("/slack/interactions", action.as_str()),
    ] {
        let form = "application/x-www-form-urlencoded";
        let r = post_signed_at(&client, path, form, body, now_secs(), "wrong").await;
        r.assert_status(401);
        let r = post_signed_at(&client, path, form, body, now_secs() - 301, SECRET).await;
        r.assert_status(401);
        let r = client
            .post(path)
            .header("content-type", form)
            .body(body.to_owned())
            .send()
            .await;
        r.assert_status(400);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    for s in ["events", "commands", "interactions"] {
        assert_eq!(metrics.requests(s, "bad_signature"), 1, "{s}");
        assert_eq!(metrics.requests(s, "stale"), 1, "{s}");
        assert_eq!(metrics.requests(s, "bad_request"), 1, "{s}");
        assert_eq!(metrics.handler_runs(s, "panic"), 0, "{s}: a handler ran");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_previous_secret_does_not_let_anyone_sign() {
    let t = MemoryTransport::new();
    let client = TestApp::new()
        .plugin(
            plugin(&t)
                .with_previous_signing_secret("")
                .with_previous_signing_secret("  "),
        )
        .build();
    let body = json!({"type": "url_verification", "challenge": "c"}).to_string();
    let r = post_signed_at(
        &client,
        "/slack/events",
        "application/json",
        &body,
        now_secs(),
        "",
    )
    .await;
    r.assert_status(401);
}

#[tokio::test]
async fn health_check_is_single_flight_and_one_attempt() {
    let t = MemoryTransport::new();
    t.respond(
        "auth.test",
        HttpReply::json(429, &json!({"ok": false})).retry_after(30),
    );
    // A slow reply, so the ten probes overlap.
    t.set_latency(Duration::from_millis(50));
    let rt = plugin(&t).start(&AppState::for_test()).unwrap();
    let h = rt.health();
    let outs = futures::future::join_all((0..10).map(|_| h.check())).await;
    assert!(outs.iter().all(|o| o.status == HealthStatus::Down));
    assert_eq!(outs[0].details["error"], "rate_limited");
    assert_eq!(
        t.requests_to("auth.test").len(),
        1,
        "one call for ten probes"
    );
}

#[tokio::test(start_paused = true)]
async fn down_result_clears_fast() {
    let t = MemoryTransport::new();
    t.respond_seq(
        "auth.test",
        vec![
            HttpReply::text(503, ""),
            HttpReply::json(200, &json!({"ok": true, "team_id": "T", "user_id": "U"})),
        ],
    );
    let rt = plugin(&t).start(&AppState::for_test()).unwrap();
    assert_eq!(rt.health().check().await.status, HealthStatus::Down);
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(
        rt.health().check().await.status,
        HealthStatus::Down,
        "cached"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(rt.health().check().await.status, HealthStatus::Up);
}

#[test]
fn plain_http_api_base_url_needs_loopback() {
    let mut c = config();
    c.api_base_url = "http://slack.example/api/".to_owned();
    assert!(c.validate().is_err());
    c.api_base_url = "http://127.0.0.1:9/api/".to_owned();
    c.validate().unwrap();
}

// ------------------------------------------------- concurrency review

#[tokio::test(flavor = "multi_thread")]
async fn routes_reply_503_after_shutdown_and_keep_retries() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let rt = plugin(&t)
        .on_event("message", move |_c, _e| {
            let tx = tx.clone();
            async move {
                tx.send(()).unwrap();
                Ok(())
            }
        })
        .start(&AppState::for_test())
        .unwrap();
    rt.shutdown().await;
    let body = event_callback("EvLate", &json!({"type": "message"})).to_string();
    assert_eq!(send(&rt, "/events", &body, Some(SECRET)).await, 503);
    assert_eq!(rt.metrics().requests("events", "shutting_down"), 1);
    assert_eq!(rt.metrics().requests("events", "duplicate"), 0);
    assert!(rx.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn late_ack_counts_once_and_late_ack_reply_posts_nothing() {
    let t = MemoryTransport::new();
    let gate = Gate::default();
    let g = gate.clone();
    let p = plugin_with(&t, slow_config()).command("/quiet", move |_c, _cmd| {
        let g = g.clone();
        async move {
            g.wait().await;
            Ok(CommandResponse::Ack)
        }
    });
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    let r = within(post_form(
        &client,
        "/slack/commands",
        &command_form("/quiet", ""),
    ))
    .await;
    r.assert_status(200);
    gate.open();
    wait_until(Duration::from_secs(5), &|| {
        metrics.late_acks("commands") == 1
    })
    .await;
    wait_until(Duration::from_secs(5), &|| {
        metrics.handler_runs("commands", "ok") == 1
    })
    .await;
    assert_eq!(metrics.in_flight(), 0);
    assert!(
        t.requests_to(RESPONSE_URL).is_empty(),
        "a late Ack posts nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_returns_only_after_the_handler_ends() {
    let t = MemoryTransport::new();
    let gate = Gate::default();
    let g = gate.clone();
    let rt = plugin(&t)
        .on_event("message", move |_c, _e| {
            let g = g.clone();
            async move {
                g.wait().await;
                Ok(())
            }
        })
        .start(&AppState::for_test())
        .unwrap();
    let body = event_callback("EvGate", &json!({"type": "message"})).to_string();
    assert_eq!(send(&rt, "/events", &body, Some(SECRET)).await, 200);
    let opened = Arc::new(AtomicBool::new(false));
    let o = Arc::clone(&opened);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        o.store(true, Ordering::SeqCst);
        gate.open();
    });
    rt.shutdown().await;
    assert!(opened.load(Ordering::SeqCst), "shutdown did not wait");
    assert_eq!(rt.in_flight(), 0);
}

// ---------------------------------------------------- protocol review

#[tokio::test(flavor = "multi_thread")]
async fn ssl_check_needs_no_signature() {
    let t = MemoryTransport::new();
    let client = TestApp::new().plugin(plugin(&t)).build();
    let r = client
        .post("/slack/commands")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("ssl_check=1&token=x")
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.text(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn block_suggestion_gets_no_options() {
    let t = MemoryTransport::new();
    let client = TestApp::new().plugin(plugin(&t)).build();
    let r = post_interaction(
        &client,
        &json!({"type": "block_suggestion", "action_id": "x"}),
    )
    .await;
    r.assert_status(200);
    assert_eq!(r.json::<Value>(), json!({"options": []}));
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_headers_reach_the_handler_and_dedup_counts() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<EventCallback>();
    let p = plugin(&t).on_event("message", move |_c, ev| {
        let tx = tx.clone();
        async move {
            tx.send(ev).unwrap();
            Ok(())
        }
    });
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    let body = event_callback("EvRetry", &json!({"type": "message"})).to_string();
    let send_retry = || async {
        let ts = now_secs();
        client
            .post("/slack/events")
            .header("x-slack-request-timestamp", &ts.to_string())
            .header(
                "x-slack-signature",
                &testing::sign(SECRET, ts, body.as_bytes()),
            )
            .header("x-slack-retry-num", "1")
            .header("x-slack-retry-reason", "http_timeout")
            .body(body.clone())
            .send()
            .await
    };
    send_retry().await.assert_status(200);
    let ev = recv(&mut rx).await;
    assert_eq!(ev.retry_num, Some(1));
    assert_eq!(ev.retry_reason.as_deref(), Some("http_timeout"));
    send_retry().await.assert_status(200);
    assert_eq!(metrics.requests("events", "duplicate"), 1);
}

#[tokio::test(start_paused = true)]
async fn trigger_calls_do_not_wait_past_the_trigger_life() {
    let t = MemoryTransport::new();
    t.respond(
        "views.open",
        HttpReply::json(429, &json!({"ok": false})).retry_after(2),
    );
    let rt = plugin(&t).start(&AppState::for_test()).unwrap();
    let e = rt
        .client()
        .views_open("trig", &json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            SlackError::RateLimited {
                retry_after_secs: Some(2)
            }
        ),
        "{e:?}"
    );
    assert_eq!(t.requests_to("views.open").len(), 1);
}

// ----------------------------------------------------- autumn review

#[test]
fn worker_role_needs_no_secret_and_no_exemption() {
    let t = MemoryTransport::new();
    let mut cfg = AutumnConfig::default();
    cfg.security.csrf.enabled = true;
    let mut c = config();
    c.signing_secret_env = "AUTUMN_SLACK_TEST_NO_SUCH_SECRET".to_owned();
    let rt = SlackPlugin::with_config(c)
        .with_bot_token(TOKEN)
        .with_transport(t)
        .start_with_role(&state_with(cfg), ProcessRole::Worker)
        .unwrap();
    assert!(rt.client().has_token());
}

#[test]
fn captcha_dev_bypass_and_json_events_pass() {
    let t = MemoryTransport::new();
    let mut cfg = AutumnConfig::default();
    cfg.bot_protection.enabled = true;
    cfg.bot_protection.dev_bypass = true;
    plugin(&t).start(&state_with(cfg.clone())).unwrap();
    // Only the form routes need the exemption. `/events` is JSON.
    cfg.bot_protection.dev_bypass = false;
    cfg.security.captcha_exempt_paths = vec![
        "/slack/commands".to_owned(),
        "/slack/interactions".to_owned(),
    ];
    plugin(&t).start(&state_with(cfg)).unwrap();
}

#[test]
fn root_mount_csrf_fix_names_exact_paths() {
    let t = MemoryTransport::new();
    let mut cfg = AutumnConfig::default();
    cfg.security.csrf.enabled = true;
    let e = plugin(&t)
        .base_path("/")
        .start(&state_with(cfg))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("\"/events\"") && e.contains("\"/interactions\""),
        "{e}"
    );
    assert!(!e.contains("Add [\"/\"]"), "{e}");
}

#[test]
fn failed_startup_stops_the_app_with_the_reason() {
    let t = MemoryTransport::new();
    let mut c = config();
    c.signing_secret_env = "AUTUMN_SLACK_TEST_NO_SUCH_SECRET_2".to_owned();
    let p = SlackPlugin::with_config(c).with_transport(t);
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(TestApp::new().plugin(p).build());
    }));
    let msg = built.expect_err("startup must fail");
    let text = msg
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| msg.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(
        text.contains("AUTUMN_SLACK_TEST_NO_SUCH_SECRET_2"),
        "{text}"
    );
}

// ------------------------------------------------------ tests review

#[tokio::test]
async fn api_calls_drop_reply_only_fields() {
    let t = MemoryTransport::new();
    t.respond(
        "chat.postMessage",
        HttpReply::json(200, &json!({"ok": true, "channel": "C", "ts": "1"})),
    );
    t.respond(
        "chat.update",
        HttpReply::json(200, &json!({"ok": true, "channel": "C", "ts": "1"})),
    );
    let rt = plugin(&t).start(&AppState::for_test()).unwrap();
    let c = rt.client();
    let reply = Message::text("x")
        .ephemeral()
        .replace_original()
        .delete_original();
    c.chat_post_message("C", &reply).await.unwrap();
    c.chat_update("C", "1", &Message::text("y").in_thread("0.1"))
        .await
        .unwrap();
    let post = t.requests_to("chat.postMessage")[0].form();
    for k in ["response_type", "replace_original", "delete_original"] {
        assert!(!post.contains_key(k), "{k} sent to the Web API");
    }
    assert!(
        !t.requests_to("chat.update")[0]
            .form()
            .contains_key("thread_ts")
    );
}

#[tokio::test(start_paused = true)]
async fn read_call_stops_after_max_attempts_on_5xx() {
    let t = MemoryTransport::new();
    t.respond("users.info", HttpReply::text(503, "busy"));
    let rt = plugin(&t).start(&AppState::for_test()).unwrap();
    let before = tokio::time::Instant::now();
    let e = rt.client().users_info("U").await.unwrap_err();
    assert!(matches!(e, SlackError::Http { status: 503, .. }), "{e:?}");
    // Default max_attempts is 3. Test backoff: 1 ms, then 2 ms.
    assert_eq!(t.requests_to("users.info").len(), 3);
    assert_eq!(rt.metrics().api_retries("users.info"), 2);
    assert_eq!(before.elapsed(), Duration::from_millis(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn handler_panic_and_error_are_counted() {
    let t = MemoryTransport::new();
    let p = plugin(&t)
        .on_event("boom", |_c, _e| async { panic!("boom") })
        .on_event("fail", |_c, _e| async { Err("no".into()) });
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    post_event(&client, &event_callback("E1", &json!({"type": "boom"})))
        .await
        .assert_status(200);
    post_event(&client, &event_callback("E2", &json!({"type": "fail"})))
        .await
        .assert_status(200);
    wait_until(Duration::from_secs(5), &|| {
        metrics.handler_runs("events", "panic") == 1 && metrics.handler_runs("events", "error") == 1
    })
    .await;
    assert_eq!(metrics.in_flight(), 0);
}

// ------------------------------------------------------ round 2 review

#[tokio::test]
async fn single_flight_holds_with_zero_cache_time() {
    let t = MemoryTransport::new();
    t.respond(
        "auth.test",
        HttpReply::json(200, &json!({"ok": true, "team_id": "T", "user_id": "U"})),
    );
    t.set_latency(Duration::from_millis(50));
    let mut c = config();
    c.health.cache_secs = 0;
    let rt = plugin_with(&t, c).start(&AppState::for_test()).unwrap();
    let h = rt.health();
    let outs = futures::future::join_all((0..10).map(|_| h.check())).await;
    assert!(outs.iter().all(|o| o.status == HealthStatus::Up));
    assert_eq!(
        t.requests_to("auth.test").len(),
        1,
        "waiters share one check"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn late_ack_counts_while_the_handler_still_runs() {
    let t = MemoryTransport::new();
    let gate = Gate::default();
    let g = gate.clone();
    let p = plugin_with(&t, slow_config()).command("/hang", move |_c, _cmd| {
        let g = g.clone();
        async move {
            g.wait().await;
            Ok(CommandResponse::Ack)
        }
    });
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    within(post_form(
        &client,
        "/slack/commands",
        &command_form("/hang", ""),
    ))
    .await
    .assert_status(200);
    // The handler still waits on the gate. The metric shows the late ack now.
    wait_until(Duration::from_secs(5), &|| {
        metrics.late_acks("commands") == 1
    })
    .await;
    assert_eq!(metrics.in_flight(), 1);
    gate.open();
}

#[tokio::test(flavor = "multi_thread")]
async fn ssl_check_has_its_own_label() {
    let t = MemoryTransport::new();
    let p = plugin(&t);
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    client
        .post("/slack/commands")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("token=x&ssl_check=1")
        .send()
        .await
        .assert_status(200);
    assert_eq!(metrics.requests("commands", "ssl_check"), 1);
    assert_eq!(metrics.requests("commands", "ok"), 0);
}

#[test]
fn client_is_ready_before_jobs_start() {
    // autumn runs state initializers before job workers. An initializer
    // after the plugin sees the client, so a `#[job]` sees it too.
    let t = MemoryTransport::new();
    let seen = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&seen);
    let _client = TestApp::new()
        .plugin(plugin(&t))
        .state_initializer(move |state| {
            s.store(
                autumn_plugin_slack::SlackClient::from_state(state).is_some(),
                Ordering::SeqCst,
            );
        })
        .build();
    assert!(seen.load(Ordering::SeqCst));
}
