//! `SlackClient` with the in-memory transport.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.

mod common;

use autumn_plugin_slack::payload::Message;
use autumn_plugin_slack::transport::{HttpReply, MemoryTransport};
use autumn_plugin_slack::{SlackClient, SlackError, SlackRuntime};
use autumn_web::AppState;
use common::*;
use serde_json::json;

fn start(t: &MemoryTransport) -> (SlackRuntime, SlackClient) {
    let rt = plugin(t).start(&AppState::for_test()).unwrap();
    let client = rt.client();
    (rt, client)
}

// --------------------------------------------------------------- calls (AC6)

#[tokio::test]
async fn post_message_sends_form_with_bearer_token() {
    let t = MemoryTransport::new();
    t.respond(
        "chat.postMessage",
        HttpReply::json(200, &json!({"ok": true, "channel": "C1", "ts": "1.5"})),
    );
    let (_rt, c) = start(&t);
    let msg = Message::text("hi")
        .blocks(vec![
            json!({"type": "section", "text": {"type": "mrkdwn", "text": "*hi*"}}),
        ])
        .in_thread("1.0");
    let r = c.chat_post_message("C1", &msg).await.unwrap();
    assert_eq!(r.channel, "C1");
    assert_eq!(r.ts, "1.5");
    let req = &t.requests_to("chat.postMessage")[0];
    assert_eq!(req.url, "https://slack.com/api/chat.postMessage");
    assert_eq!(
        req.header("authorization").as_deref(),
        Some("Bearer xoxb-test-token")
    );
    assert!(
        req.header("content-type")
            .unwrap()
            .starts_with("application/x-www-form-urlencoded")
    );
    let form = req.form();
    assert_eq!(form["channel"], "C1");
    assert_eq!(form["text"], "hi");
    assert_eq!(form["thread_ts"], "1.0");
    // Nested values go as JSON text.
    let blocks: serde_json::Value = serde_json::from_str(&form["blocks"]).unwrap();
    assert_eq!(blocks[0]["text"]["text"], "*hi*");
}

#[tokio::test]
async fn typed_calls_hit_their_methods() {
    let t = MemoryTransport::new();
    t.respond(
        "chat.update",
        HttpReply::json(200, &json!({"ok": true, "channel": "C1", "ts": "1.5"})),
    );
    t.respond(
        "chat.postEphemeral",
        HttpReply::json(200, &json!({"ok": true, "message_ts": "1.6"})),
    );
    t.respond(
        "users.info",
        HttpReply::json(
            200,
            &json!({"ok": true, "user": {"id": "U1", "name": "ada"}}),
        ),
    );
    t.respond(
        "views.open",
        HttpReply::json(200, &json!({"ok": true, "view": {"id": "V1"}})),
    );
    let (_rt, c) = start(&t);
    let view = json!({"type": "modal", "callback_id": "signup"});
    assert_eq!(
        c.chat_update("C1", "1.5", &Message::text("edit"))
            .await
            .unwrap()
            .ts,
        "1.5"
    );
    assert_eq!(
        c.chat_post_ephemeral("C1", "U1", &Message::text("psst"))
            .await
            .unwrap(),
        "1.6"
    );
    c.chat_delete("C1", "1.5").await.unwrap();
    c.reactions_add("C1", "1.5", "thumbsup").await.unwrap();
    assert_eq!(c.views_open("trig", &view).await.unwrap()["id"], "V1");
    c.views_update("V1", &view, Some("hash1")).await.unwrap();
    c.views_push("trig", &view).await.unwrap();
    c.views_publish("U1", &json!({"type": "home"}))
        .await
        .unwrap();
    assert_eq!(c.users_info("U1").await.unwrap()["name"], "ada");
    for m in [
        "chat.update",
        "chat.postEphemeral",
        "chat.delete",
        "reactions.add",
        "views.open",
        "views.update",
        "views.push",
        "views.publish",
        "users.info",
    ] {
        assert_eq!(t.requests_to(m).len(), 1, "{m}");
    }
    let upd = t.requests_to("views.update")[0].form();
    assert_eq!(upd["view_id"], "V1");
    assert_eq!(upd["hash"], "hash1");
    assert_eq!(t.requests_to("reactions.add")[0].form()["name"], "thumbsup");
    assert_eq!(t.requests_to("chat.postEphemeral")[0].form()["user"], "U1");
}

#[tokio::test]
async fn auth_test_is_typed() {
    let t = MemoryTransport::new();
    t.respond(
        "auth.test",
        HttpReply::json(200, &json!({"ok": true, "team_id": "T1", "user_id": "UB", "bot_id": "B1", "team": "Acme", "url": "https://acme.slack.com/"})),
    );
    let (_rt, c) = start(&t);
    let a = c.auth_test().await.unwrap();
    assert_eq!(a.team_id, "T1");
    assert_eq!(a.user_id, "UB");
    assert_eq!(a.bot_id.as_deref(), Some("B1"));
}

#[tokio::test]
async fn ok_false_gives_typed_api_error() {
    let t = MemoryTransport::new();
    t.respond(
        "chat.postMessage",
        HttpReply::json(
            200,
            &json!({"ok": false, "error": "missing_scope", "needed": "chat:write"}),
        ),
    );
    let (_rt, c) = start(&t);
    let e = c
        .chat_post_message("C1", &Message::text("x"))
        .await
        .unwrap_err();
    match &e {
        SlackError::Api {
            method,
            code,
            needed,
        } => {
            assert_eq!(method, "chat.postMessage");
            assert_eq!(code, "missing_scope");
            assert_eq!(needed.as_deref(), Some("chat:write"));
        }
        other => panic!("wrong error {other:?}"),
    }
    assert_eq!(e.slack_code(), Some("missing_scope"));
    // The token never shows in errors.
    assert!(!format!("{e} {e:?}").contains(TOKEN));
}

#[tokio::test]
async fn generic_call_and_bad_params() {
    let t = MemoryTransport::new();
    t.respond(
        "pins.add",
        HttpReply::json(200, &json!({"ok": true, "x": 1})),
    );
    let (_rt, c) = start(&t);
    let v = c
        .call(
            "pins.add",
            &json!({"channel": "C1", "timestamp": "1.2", "skip": null, "n": 3, "flag": true}),
        )
        .await
        .unwrap();
    assert_eq!(v["x"], 1);
    let form = t.requests_to("pins.add")[0].form();
    assert_eq!(form["n"], "3");
    assert_eq!(form["flag"], "true");
    assert!(!form.contains_key("skip"));
    assert!(matches!(
        c.call("pins.add", &json!([1, 2])).await,
        Err(SlackError::Config(_))
    ));
    assert!(matches!(
        c.call("../evil", &json!({})).await,
        Err(SlackError::Config(_))
    ));
}

#[tokio::test]
async fn non_json_and_http_errors() {
    let t = MemoryTransport::new();
    t.respond("a.b", HttpReply::text(200, "<html>"));
    t.respond("c.d", HttpReply::text(404, "nope"));
    let (_rt, c) = start(&t);
    assert!(matches!(
        c.call("a.b", &json!({})).await,
        Err(SlackError::Decode(_))
    ));
    assert!(matches!(
        c.call("c.d", &json!({})).await,
        Err(SlackError::Http { status: 404, .. })
    ));
}

#[tokio::test]
async fn paginate_follows_next_cursor() {
    let t = MemoryTransport::new();
    t.respond_seq(
        "conversations.list",
        vec![
            HttpReply::json(200, &json!({"ok": true, "channels": [{"id": "C1"}, {"id": "C2"}], "response_metadata": {"next_cursor": "abc"}})),
            HttpReply::json(200, &json!({"ok": true, "channels": [{"id": "C3"}], "response_metadata": {"next_cursor": ""}})),
        ],
    );
    let (_rt, c) = start(&t);
    let all = c
        .paginate("conversations.list", &json!({"limit": 2}), "channels", 10)
        .await
        .unwrap();
    let ids: Vec<_> = all.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["C1", "C2", "C3"]);
    let reqs = t.requests_to("conversations.list");
    assert_eq!(reqs.len(), 2);
    assert!(!reqs[0].form().contains_key("cursor"));
    assert_eq!(reqs[1].form()["cursor"], "abc");
}

#[tokio::test]
async fn paginate_stops_at_max_pages() {
    let t = MemoryTransport::new();
    t.respond("users.list", HttpReply::json(200, &json!({"ok": true, "members": [{"id": "U"}], "response_metadata": {"next_cursor": "again"}})));
    let (_rt, c) = start(&t);
    let all = c
        .paginate("users.list", &json!({}), "members", 3)
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(t.requests_to("users.list").len(), 3);
}

#[tokio::test]
async fn missing_token_gives_config_error() {
    let t = MemoryTransport::new();
    let rt = autumn_plugin_slack::SlackPlugin::with_config(no_token_config())
        .with_signing_secret(SECRET)
        .with_transport(t.clone())
        .start(&AppState::for_test())
        .unwrap();
    let e = rt.client().auth_test().await.unwrap_err();
    assert!(matches!(e, SlackError::Config(_)), "{e:?}");
    assert!(t.requests().is_empty());
}

// --------------------------------------------------------------- retry (AC7)

#[tokio::test(start_paused = true)]
async fn rate_limit_waits_retry_after_then_succeeds() {
    let t = MemoryTransport::new();
    t.respond_seq(
        "chat.postMessage",
        vec![
            HttpReply::json(429, &json!({"ok": false, "error": "ratelimited"})).retry_after(3),
            HttpReply::json(200, &json!({"ok": true, "channel": "C1", "ts": "2.0"})),
        ],
    );
    let (rt, c) = start(&t);
    let before = tokio::time::Instant::now();
    let r = c
        .chat_post_message("C1", &Message::text("x"))
        .await
        .unwrap();
    assert_eq!(r.ts, "2.0");
    assert!(before.elapsed() >= std::time::Duration::from_secs(3));
    assert_eq!(t.requests_to("chat.postMessage").len(), 2);
    assert_eq!(rt.metrics().api_retries("chat.postMessage"), 1);
}

#[tokio::test(start_paused = true)]
async fn rate_limit_over_cap_gives_rate_limited_error() {
    let t = MemoryTransport::new();
    t.respond(
        "chat.postMessage",
        HttpReply::json(429, &json!({"ok": false})).retry_after(3600),
    );
    let (_rt, c) = start(&t);
    let e = c
        .chat_post_message("C1", &Message::text("x"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            SlackError::RateLimited {
                retry_after_secs: Some(3600)
            }
        ),
        "{e:?}"
    );
    assert_eq!(t.requests_to("chat.postMessage").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn rate_limit_attempts_are_bounded() {
    let t = MemoryTransport::new();
    t.respond(
        "chat.postMessage",
        HttpReply::json(429, &json!({"ok": false})).retry_after(1),
    );
    let (_rt, c) = start(&t);
    let e = c
        .chat_post_message("C1", &Message::text("x"))
        .await
        .unwrap_err();
    assert!(matches!(e, SlackError::RateLimited { .. }));
    // Default max_attempts is 3.
    assert_eq!(t.requests_to("chat.postMessage").len(), 3);
}

#[tokio::test(start_paused = true)]
async fn server_error_retries_read_but_not_write() {
    let t = MemoryTransport::new();
    t.respond_seq(
        "users.info",
        vec![
            HttpReply::text(503, "busy"),
            HttpReply::json(200, &json!({"ok": true, "user": {"id": "U1"}})),
        ],
    );
    t.respond("chat.postMessage", HttpReply::text(500, "oops"));
    let (_rt, c) = start(&t);
    assert_eq!(c.users_info("U1").await.unwrap()["id"], "U1");
    assert_eq!(t.requests_to("users.info").len(), 2);
    let e = c
        .chat_post_message("C1", &Message::text("x"))
        .await
        .unwrap_err();
    assert!(matches!(e, SlackError::Http { status: 500, .. }));
    assert_eq!(
        t.requests_to("chat.postMessage").len(),
        1,
        "write retried after 5xx"
    );
}

#[tokio::test(start_paused = true)]
async fn network_error_retries_read_only() {
    let t = MemoryTransport::new();
    t.fail_next("auth.test", 1);
    t.respond(
        "auth.test",
        HttpReply::json(200, &json!({"ok": true, "team_id": "T1", "user_id": "U"})),
    );
    t.fail_next("chat.delete", 1);
    let (_rt, c) = start(&t);
    assert_eq!(c.auth_test().await.unwrap().team_id, "T1");
    assert_eq!(t.requests_to("auth.test").len(), 2);
    assert!(matches!(
        c.chat_delete("C1", "1").await,
        Err(SlackError::Transport(_))
    ));
    assert_eq!(t.requests_to("chat.delete").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn call_read_marks_custom_method_idempotent() {
    let t = MemoryTransport::new();
    t.respond_seq(
        "team.info",
        vec![
            HttpReply::text(502, ""),
            HttpReply::json(200, &json!({"ok": true})),
        ],
    );
    let (_rt, c) = start(&t);
    c.call_read("team.info", &json!({})).await.unwrap();
    assert_eq!(t.requests_to("team.info").len(), 2);
}

// ---------------------------------------------------------- response_url (AC8)

#[tokio::test]
async fn respond_posts_json_to_allowed_host_only() {
    let t = MemoryTransport::new();
    let (_rt, c) = start(&t);
    c.respond(RESPONSE_URL, &Message::text("later").ephemeral())
        .await
        .unwrap();
    let req = &t.requests_to(RESPONSE_URL)[0];
    assert_eq!(req.json()["response_type"], "ephemeral");
    assert!(
        req.header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    assert!(req.header("authorization").is_none());
    for bad in [
        "http://hooks.slack.com/commands/x",
        "https://evil.example/commands/x",
        "https://hooks.slack.com.evil.example/x",
        "https://user@evil.example/x",
        "https://hooks.slack.com:8443/x",
        "not a url",
        "https://169.254.169.254/latest/meta-data",
    ] {
        let e = c.respond(bad, &Message::text("x")).await.unwrap_err();
        assert!(matches!(e, SlackError::BadResponseUrl), "{bad}: {e:?}");
    }
    assert_eq!(t.requests().len(), 1);
}

#[tokio::test]
async fn respond_reports_http_failure() {
    let t = MemoryTransport::new();
    t.respond(RESPONSE_URL, HttpReply::text(404, "expired_url"));
    let (_rt, c) = start(&t);
    let e = c
        .respond(RESPONSE_URL, &Message::text("x"))
        .await
        .unwrap_err();
    assert!(matches!(e, SlackError::Http { status: 404, .. }), "{e:?}");
}
