//! Plugin install, startup checks, health, metrics, and drain.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use autumn_plugin_slack::payload::CommandResponse;
use autumn_plugin_slack::transport::{HttpReply, MemoryTransport};
use autumn_plugin_slack::{SlackError, SlackPlugin};
use autumn_web::AppState;
use autumn_web::actuator::{HealthIndicator, HealthStatus, IndicatorGroup, MetricsSource};
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use common::*;
use serde_json::json;

fn state_with(cfg: AutumnConfig) -> AppState {
    let s = AppState::for_test();
    s.insert_extension(cfg);
    s
}

/// Sends a signed request to the runtime router. Returns the status.
async fn oneshot(rt: &autumn_plugin_slack::SlackRuntime, path: &str, body: &str) -> u16 {
    use autumn_web::reexports::axum::body::Body;
    use autumn_web::reexports::http::Request;
    use tower::ServiceExt as _;
    let ts = now_secs();
    let req = Request::post(path)
        .header("x-slack-request-timestamp", ts.to_string())
        .header(
            "x-slack-signature",
            autumn_plugin_slack::testing::sign(SECRET, ts, body.as_bytes()),
        )
        .body(Body::from(body.to_owned()))
        .unwrap();
    rt.router().oneshot(req).await.unwrap().status().as_u16()
}

fn auth_ok(t: &MemoryTransport) {
    t.respond(
        "auth.test",
        HttpReply::json(200, &json!({"ok": true, "team_id": "T1", "user_id": "UB"})),
    );
}

// ----------------------------------------------------------------- install (AC1)

#[tokio::test(flavor = "multi_thread")]
async fn plugin_installs_with_one_call() {
    let t = MemoryTransport::new();
    auth_ok(&t);
    let client = TestApp::new().plugin(plugin(&t)).build();
    let state = client.state();
    assert!(autumn_plugin_slack::SlackClient::from_state(state).is_some());
    let body = client.get("/actuator/health").send().await.text();
    assert!(body.contains("slack"), "{body}");
    assert!(body.contains("UP"), "{body}");
    let url = post_event(
        &client,
        &json!({"type": "url_verification", "challenge": "c"}),
    )
    .await;
    url.assert_status(200);
}

#[test]
fn plugin_name_contract_and_config_section() {
    use autumn_web::plugin::Plugin;
    let p = SlackPlugin::new();
    assert_eq!(p.name(), "autumn-plugin-slack");
    assert!(format!("{p:?}").contains("SlackPlugin"));
    let contract = p.contract().unwrap();
    assert!(format!("{contract:?}").contains("0.8"), "{contract:?}");
    let app = autumn_web::app().plugin(SlackPlugin::default());
    assert!(app.has_plugin("autumn-plugin-slack"));
    assert!(app.has_config_section("slack"));
}

#[test]
fn plugin_declares_its_routes() {
    use autumn_web::route_listing::{RouteClassification, RouteSource};
    let app = autumn_web::app().plugin(SlackPlugin::new().base_path("/hooks/slack"));
    let routes = app.plugin_route_infos().unwrap();
    for p in [
        "/hooks/slack/events",
        "/hooks/slack/commands",
        "/hooks/slack/interactions",
    ] {
        let r = routes
            .iter()
            .find(|r| r.path == p)
            .unwrap_or_else(|| panic!("no route {p}"));
        assert_eq!(r.method, "POST");
        assert_eq!(r.classification, RouteClassification::Public, "{p}");
        assert_eq!(
            r.source,
            RouteSource::Plugin("autumn-plugin-slack".to_owned())
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_base_path_serves_routes() {
    let t = MemoryTransport::new();
    let client = TestApp::new()
        .plugin(plugin(&t).base_path("/hooks/slack/"))
        .build();
    let body = json!({"type": "url_verification", "challenge": "c"}).to_string();
    post_signed(&client, "/hooks/slack/events", "application/json", &body)
        .await
        .assert_status(200);
}

// ---------------------------------------------------------- startup checks (AC11)

#[tokio::test]
async fn missing_signing_secret_fails_start() {
    let t = MemoryTransport::new();
    let mut c = config();
    c.signing_secret_env = "AUTUMN_SLACK_TEST_UNSET_SECRET".to_owned();
    let e = SlackPlugin::with_config(c)
        .with_transport(t)
        .start(&AppState::for_test())
        .unwrap_err();
    assert!(
        e.to_string().contains("AUTUMN_SLACK_TEST_UNSET_SECRET"),
        "{e}"
    );
}

#[tokio::test]
async fn invalid_config_fails_start() {
    let t = MemoryTransport::new();
    let mut c = config();
    c.ack_timeout_ms = 5_000;
    let e = plugin_with(&t, c).start(&AppState::for_test()).unwrap_err();
    assert!(matches!(e, SlackError::Config(_)));
}

#[tokio::test]
async fn duplicate_or_bad_handlers_fail_start() {
    let t = MemoryTransport::new();
    let ok = |_ctx, _cmd| async move { Ok(CommandResponse::Ack) };
    let e = plugin(&t)
        .command("/a", ok)
        .command("/a", ok)
        .start(&AppState::for_test())
        .unwrap_err();
    assert!(e.to_string().contains("/a"), "{e}");
    let e = plugin(&t)
        .command("noslash", ok)
        .start(&AppState::for_test())
        .unwrap_err();
    assert!(e.to_string().contains("noslash"), "{e}");
    let e = plugin(&t)
        .action("x", |_c, _a| async move { Ok(None) })
        .action("x", |_c, _a| async move { Ok(None) })
        .start(&AppState::for_test())
        .unwrap_err();
    assert!(e.to_string().contains('x'), "{e}");
}

#[tokio::test]
async fn csrf_without_exemption_fails_start_with_fix() {
    let t = MemoryTransport::new();
    let mut cfg = AutumnConfig::default();
    cfg.security.csrf.enabled = true;
    let state = state_with(cfg.clone());
    let e = plugin(&t).start(&state).unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("exempt_paths") && msg.contains("/slack"),
        "{msg}"
    );
    // With the exemption, start works.
    cfg.security.csrf.exempt_paths = vec!["/slack/".to_owned()];
    let state = state_with(cfg.clone());
    plugin(&t).start(&state).unwrap();
    // A sibling prefix is not an exemption.
    cfg.security.csrf.exempt_paths = vec!["/sla".to_owned()];
    let state = state_with(cfg);
    assert!(plugin(&t).start(&state).is_err());
}

#[tokio::test]
async fn captcha_without_exemption_fails_start() {
    let t = MemoryTransport::new();
    let mut cfg = AutumnConfig::default();
    cfg.bot_protection.enabled = true;
    let state = state_with(cfg.clone());
    let e = plugin(&t).start(&state).unwrap_err();
    assert!(e.to_string().contains("captcha_exempt_paths"), "{e}");
    cfg.security.captcha_exempt_paths = vec!["/slack".to_owned()];
    let state = state_with(cfg);
    plugin(&t).start(&state).unwrap();
}

// ---------------------------------------------------------- health (AC10)

#[tokio::test]
async fn health_unknown_before_start_and_without_token() {
    let t = MemoryTransport::new();
    let rt = SlackPlugin::with_config(config())
        .with_signing_secret(SECRET)
        .with_transport(t)
        .start(&AppState::for_test())
        .unwrap();
    let out = rt.health().check().await;
    assert_eq!(out.status, HealthStatus::Unknown);
    assert!(matches!(rt.health().group(), IndicatorGroup::HealthOnly));
}

#[tokio::test]
async fn health_up_down_cached_and_no_secrets() {
    let t = MemoryTransport::new();
    t.respond(
        "auth.test",
        HttpReply::json(200, &json!({"ok": false, "error": "invalid_auth"})),
    );
    let rt = plugin(&t)
        .readiness(true)
        .start(&AppState::for_test())
        .unwrap();
    let h = rt.health();
    assert!(matches!(h.group(), IndicatorGroup::Readiness));
    let out = h.check().await;
    assert_eq!(out.status, HealthStatus::Down);
    assert_eq!(out.details["error"], "invalid_auth");
    let text = serde_json::to_string(&out.details).unwrap();
    assert!(!text.contains(TOKEN) && !text.contains(SECRET));
    // Cached: no second call inside cache_secs.
    h.check().await;
    assert_eq!(t.requests_to("auth.test").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn health_cache_expires() {
    let t = MemoryTransport::new();
    auth_ok(&t);
    let rt = plugin(&t).start(&AppState::for_test()).unwrap();
    assert_eq!(rt.health().check().await.status, HealthStatus::Up);
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(rt.health().check().await.status, HealthStatus::Up);
    assert_eq!(t.requests_to("auth.test").len(), 2);
}

// ---------------------------------------------------------- metrics (AC10)

#[tokio::test(flavor = "multi_thread")]
async fn metrics_count_requests_handlers_and_api_calls() {
    let t = MemoryTransport::new();
    let p = plugin(&t).command(
        "/x",
        |ctx: autumn_plugin_slack::SlackContext, _c| async move {
            ctx.client().call("pins.add", &json!({})).await?;
            Ok(CommandResponse::Ack)
        },
    );
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    post_form(&client, "/slack/commands", &command_form("/x", ""))
        .await
        .assert_status(200);
    post_signed_at(
        &client,
        "/slack/events",
        "application/json",
        "{}",
        now_secs(),
        "bad",
    )
    .await
    .assert_status(401);
    assert_eq!(metrics.requests("commands", "ok"), 1);
    assert_eq!(metrics.requests("events", "bad_signature"), 1);
    assert_eq!(metrics.handler_runs("commands", "ok"), 1);
    assert_eq!(metrics.api_calls("pins.add", "ok"), 1);
    let families = metrics.collect();
    let names: std::collections::HashSet<_> = families.iter().map(|f| f.name.clone()).collect();
    assert_eq!(names.len(), families.len(), "family names are unique");
    for n in [
        "slack_requests_total",
        "slack_handler_runs_total",
        "slack_api_calls_total",
        "slack_api_retries_total",
        "slack_handlers_in_flight",
    ] {
        assert!(names.contains(n), "{n}");
    }
    let prom = client.get("/actuator/prometheus").send().await.text();
    assert!(prom.contains("slack_requests_total"), "{prom}");
}

#[tokio::test(flavor = "multi_thread")]
async fn metric_labels_never_hold_slack_data() {
    let t = MemoryTransport::new();
    let p = plugin(&t);
    let metrics = p.metrics();
    let client = TestApp::new().plugin(p).build();
    post_event(
        &client,
        &event_callback("EvLbl", &json!({"type": "secret_event_type_xyz"})),
    )
    .await;
    post_form(
        &client,
        "/slack/commands",
        &command_form("/secret_command_xyz", ""),
    )
    .await;
    let all = format!("{:?}", metrics.collect());
    assert!(!all.contains("secret_"), "{all}");
}

// ------------------------------------------------------------- drain (AC9)

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_waits_for_in_flight_handlers() {
    let t = MemoryTransport::new();
    let done = Arc::new(AtomicU32::new(0));
    let d = Arc::clone(&done);
    let gate = Gate::default();
    let g = gate.clone();
    let rt = plugin(&t)
        .on_event("message", move |_ctx, _ev| {
            let (d, g) = (Arc::clone(&d), g.clone());
            async move {
                g.wait().await;
                d.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .start(&AppState::for_test())
        .unwrap();
    let body = event_callback("EvDrain", &json!({"type": "message"})).to_string();
    let status = oneshot(&rt, "/events", &body).await;
    assert_eq!(status, 200);
    // The gate is closed: the handler is in flight.
    assert_eq!(rt.in_flight(), 1);
    assert_eq!(rt.metrics().in_flight(), 1);
    // Open the gate a little after shutdown starts to wait.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        gate.open();
    });
    rt.shutdown().await;
    assert_eq!(done.load(Ordering::SeqCst), 1);
    assert_eq!(rt.in_flight(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_gives_up_after_drain_timeout() {
    let t = MemoryTransport::new();
    let mut c = config();
    c.drain_timeout_secs = 1;
    let rt = plugin_with(&t, c)
        .on_event("message", |_ctx, _ev| async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
        .start(&AppState::for_test())
        .unwrap();
    let body = event_callback("EvStuck", &json!({"type": "message"})).to_string();
    assert_eq!(oneshot(&rt, "/events", &body).await, 200);
    let start = std::time::Instant::now();
    rt.shutdown().await;
    assert!(start.elapsed() < Duration::from_secs(5));
}
