//! Inbound Slack requests through the full autumn middleware stack.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.

mod common;

use std::time::Duration;

use autumn_plugin_slack::payload::{
    BlockAction, CommandResponse, EventCallback, Message, MessageEvent, Shortcut, ShortcutKind,
    SlashCommand, ViewPayload, ViewResponse,
};
use autumn_plugin_slack::transport::{HttpReply, MemoryTransport};
use autumn_plugin_slack::{HandlerError, SlackContext, SlackPlugin};
use autumn_web::test::{TestApp, TestClient};
use common::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;

fn app(p: SlackPlugin) -> TestClient {
    TestApp::new().plugin(p).build()
}

// ------------------------------------------------------------ verification (AC2)

#[tokio::test(flavor = "multi_thread")]
async fn missing_headers_give_400() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    let r = client
        .post("/slack/events")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await;
    r.assert_status(400);
    // Signature present, timestamp missing.
    let r = client
        .post("/slack/events")
        .header("x-slack-signature", "v0=00")
        .body("{}")
        .send()
        .await;
    r.assert_status(400);
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_signature_gives_400() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    let r = client
        .post("/slack/events")
        .header("x-slack-request-timestamp", &now_secs().to_string())
        .header("x-slack-signature", "v1=abc")
        .body("{}")
        .send()
        .await;
    r.assert_status(400);
    let r = client
        .post("/slack/events")
        .header("x-slack-request-timestamp", "yesterday")
        .header("x-slack-signature", "v0=abc")
        .body("{}")
        .send()
        .await;
    r.assert_status(400);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_secret_gives_401_and_no_handler_runs() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let client = app(plugin(&t).on_event("app_mention", move |_ctx, _ev| {
        let tx = tx.clone();
        async move {
            tx.send(()).unwrap();
            Ok(())
        }
    }));
    let body = event_callback("Ev1", &json!({"type": "app_mention", "text": "hi"})).to_string();
    let r = post_signed_at(
        &client,
        "/slack/events",
        "application/json",
        &body,
        now_secs(),
        "wrong-secret",
    )
    .await;
    r.assert_status(401);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rx.try_recv().is_err(), "handler must not run");
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_body_gives_401() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    let ts = now_secs();
    let sig = autumn_plugin_slack::testing::sign(SECRET, ts, b"{\"type\":\"a\"}");
    let r = client
        .post("/slack/events")
        .header("x-slack-request-timestamp", &ts.to_string())
        .header("x-slack-signature", &sig)
        .body("{\"type\": \"a\"}")
        .send()
        .await;
    r.assert_status(401);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_timestamp_gives_401_and_tolerance_edge_passes() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    let body = json!({"type": "url_verification", "challenge": "c"}).to_string();
    let r = post_signed_at(
        &client,
        "/slack/events",
        "application/json",
        &body,
        now_secs() - 301,
        SECRET,
    )
    .await;
    r.assert_status(401);
    let r = post_signed_at(
        &client,
        "/slack/events",
        "application/json",
        &body,
        now_secs() + 301,
        SECRET,
    )
    .await;
    r.assert_status(401);
    let r = post_signed_at(
        &client,
        "/slack/events",
        "application/json",
        &body,
        now_secs() - 300,
        SECRET,
    )
    .await;
    r.assert_status(200);
}

#[tokio::test(flavor = "multi_thread")]
async fn previous_secret_verifies_during_rotation() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t).with_previous_signing_secret(OLD_SECRET));
    let body = json!({"type": "url_verification", "challenge": "c"}).to_string();
    let r = post_signed_at(
        &client,
        "/slack/events",
        "application/json",
        &body,
        now_secs(),
        OLD_SECRET,
    )
    .await;
    r.assert_status(200);
}

#[tokio::test(flavor = "multi_thread")]
async fn too_large_body_gives_413() {
    let t = MemoryTransport::new();
    let mut c = config();
    c.max_body_bytes = 64;
    let client = app(plugin_with(&t, c));
    let body = format!(
        "{{\"type\":\"url_verification\",\"challenge\":\"{}\"}}",
        "x".repeat(100)
    );
    let r = post_signed(&client, "/slack/events", "application/json", &body).await;
    r.assert_status(413);
}

// ----------------------------------------------------------------- events (AC3)

#[tokio::test(flavor = "multi_thread")]
async fn url_verification_echoes_challenge() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    let r = post_event(&client, &json!({"type": "url_verification", "token": "x", "challenge": "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"})).await;
    r.assert_status(200);
    let v: Value = r.json();
    assert_eq!(
        v["challenge"],
        "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn event_callback_runs_typed_handler_after_ack() {
    let t = MemoryTransport::new();
    t.respond(
        "chat.postMessage",
        HttpReply::json(200, &json!({"ok": true, "channel": "C1", "ts": "1.3"})),
    );
    let (tx, mut rx) = mpsc::unbounded_channel::<(EventCallback, MessageEvent)>();
    let client = app(plugin(&t).on_event(
        "app_mention",
        move |ctx: SlackContext, ev: EventCallback| {
            let tx = tx.clone();
            async move {
                // The handler gets a working client.
                ctx.client()
                    .chat_post_message("C1", &Message::text("pong"))
                    .await?;
                let msg: MessageEvent = ev.parse()?;
                tx.send((ev, msg)).unwrap();
                Ok(())
            }
        },
    ));
    let r = post_event(
        &client,
        &event_callback("Ev1", &json!({"type": "app_mention", "user": "U1", "text": "<@B1> ping", "channel": "C1", "ts": "1.2"})),
    )
    .await;
    r.assert_status(200);
    let (ev, msg) = recv(&mut rx).await;
    assert_eq!(ev.event_id, "Ev1");
    assert_eq!(ev.team_id.as_deref(), Some("T1"));
    assert_eq!(ev.event_type(), "app_mention");
    assert_eq!(msg.text.as_deref(), Some("<@B1> ping"));
    assert_eq!(msg.channel.as_deref(), Some("C1"));
    let calls = t.requests_to("chat.postMessage");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].form()["channel"], "C1");
}

#[tokio::test(flavor = "multi_thread")]
async fn ack_does_not_wait_for_slow_event_handler() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let gate = Gate::default();
    let g = gate.clone();
    let client = app(plugin(&t).on_event("message", move |_ctx, _ev| {
        let (tx, g) = (tx.clone(), g.clone());
        async move {
            g.wait().await;
            tx.send(()).unwrap();
            Ok(())
        }
    }));
    // The gate is closed, so the handler cannot end. The ack must not wait.
    within(post_event(
        &client,
        &event_callback("Ev2", &json!({"type": "message"})),
    ))
    .await
    .assert_status(200);
    assert!(rx.try_recv().is_err());
    gate.open();
    recv(&mut rx).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retried_event_id_runs_handler_once() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<u32>>();
    let client = app(
        plugin(&t).on_event("message", move |_ctx, ev: EventCallback| {
            let tx = tx.clone();
            async move {
                tx.send(ev.retry_num).unwrap();
                Ok(())
            }
        }),
    );
    let body = event_callback("EvDup", &json!({"type": "message"})).to_string();
    post_signed(&client, "/slack/events", "application/json", &body)
        .await
        .assert_status(200);
    let ts = now_secs();
    let sig = autumn_plugin_slack::testing::sign(SECRET, ts, body.as_bytes());
    let r = client
        .post("/slack/events")
        .header("x-slack-request-timestamp", &ts.to_string())
        .header("x-slack-signature", &sig)
        .header("x-slack-retry-num", "1")
        .header("x-slack-retry-reason", "http_timeout")
        .body(body.clone())
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(recv(&mut rx).await, None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err(), "duplicate ran the handler");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_and_rate_limited_callbacks_get_200() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    post_event(
        &client,
        &event_callback("Ev3", &json!({"type": "team_join"})),
    )
    .await
    .assert_status(200);
    post_event(
        &client,
        &json!({"type": "app_rate_limited", "team_id": "T1", "minute_rate_limited": 1}),
    )
    .await
    .assert_status(200);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_event_json_gives_400() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    post_signed(&client, "/slack/events", "application/json", "not json")
        .await
        .assert_status(400);
    // event_callback with no event_id.
    post_event(
        &client,
        &json!({"type": "event_callback", "event": {"type": "message"}}),
    )
    .await
    .assert_status(400);
}

// ---------------------------------------------------------- slash commands (AC4)

#[tokio::test(flavor = "multi_thread")]
async fn command_replies_in_time_with_ephemeral_message() {
    let t = MemoryTransport::new();
    let client = app(
        plugin(&t).command("/deploy", |_ctx, cmd: SlashCommand| async move {
            assert_eq!(cmd.user_id, "U1");
            assert_eq!(cmd.channel_id, "C1");
            assert_eq!(cmd.trigger_id, "trig.1");
            Ok(CommandResponse::ephemeral(format!(
                "Deploying {}",
                cmd.text
            )))
        }),
    );
    let r = post_form(
        &client,
        "/slack/commands",
        &command_form("/deploy", "api v2"),
    )
    .await;
    r.assert_status(200);
    let v: Value = r.json();
    assert_eq!(v["response_type"], "ephemeral");
    assert_eq!(v["text"], "Deploying api v2");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_in_channel_with_blocks_and_empty_ack() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t)
        .command("/say", |_ctx, cmd: SlashCommand| async move {
            Ok(CommandResponse::in_channel(
                Message::text(cmd.text.clone()).blocks(vec![json!({"type": "divider"})]),
            ))
        })
        .command(
            "/quiet",
            |_ctx, _cmd| async move { Ok(CommandResponse::Ack) },
        ));
    let r = post_form(&client, "/slack/commands", &command_form("/say", "hello")).await;
    let v: Value = r.json();
    assert_eq!(v["response_type"], "in_channel");
    assert_eq!(v["blocks"][0]["type"], "divider");
    let r = post_form(&client, "/slack/commands", &command_form("/quiet", "")).await;
    r.assert_status(200);
    assert_eq!(r.text(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_command_acks_then_posts_to_response_url() {
    let t = MemoryTransport::new();
    let gate = Gate::default();
    let g = gate.clone();
    let client = app(
        plugin_with(&t, slow_config()).command("/report", move |_ctx, _cmd| {
            let g = g.clone();
            async move {
                g.wait().await;
                Ok(CommandResponse::in_channel("Report ready"))
            }
        }),
    );
    let r = within(post_form(
        &client,
        "/slack/commands",
        &command_form("/report", ""),
    ))
    .await;
    r.assert_status(200);
    assert_eq!(r.text(), "");
    assert!(t.requests_to(RESPONSE_URL).is_empty());
    gate.open();
    wait_until(Duration::from_secs(5), &|| {
        !t.requests_to(RESPONSE_URL).is_empty()
    })
    .await;
    let posted = &t.requests_to(RESPONSE_URL)[0];
    let v = posted.json();
    assert_eq!(v["response_type"], "in_channel");
    assert_eq!(v["text"], "Report ready");
    // The bot token never goes to a response URL.
    assert!(posted.header("authorization").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_command_gets_ephemeral_text() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    let r = post_form(&client, "/slack/commands", &command_form("/nope", "")).await;
    r.assert_status(200);
    let v: Value = r.json();
    assert_eq!(v["response_type"], "ephemeral");
    assert!(v["text"].as_str().unwrap().contains("not available"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ssl_check_gets_200() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    post_form(
        &client,
        "/slack/commands",
        &[("ssl_check", "1"), ("token", "x")],
    )
    .await
    .assert_status(200);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_command_form_gives_400() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    post_form(&client, "/slack/commands", &[("text", "no command field")])
        .await
        .assert_status(400);
}

// ---------------------------------------------------- handler failures (AC9)

#[tokio::test(flavor = "multi_thread")]
async fn command_error_and_panic_give_error_text_not_500() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t)
        .command("/fail", |_ctx, _cmd| async move {
            Err::<CommandResponse, HandlerError>("db down".into())
        })
        .command("/boom", |_ctx, _cmd| async move {
            panic!("boom");
            #[allow(unreachable_code)]
            Ok(CommandResponse::Ack)
        }));
    for name in ["/fail", "/boom"] {
        let r = post_form(&client, "/slack/commands", &command_form(name, "")).await;
        r.assert_status(200);
        let v: Value = r.json();
        assert_eq!(v["response_type"], "ephemeral");
        assert!(v["text"].as_str().unwrap().starts_with("Sorry"), "{v}");
        assert!(!v.to_string().contains("db down"), "internal error leaked");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_command_error_posts_error_text() {
    let t = MemoryTransport::new();
    let gate = Gate::default();
    let g = gate.clone();
    let client = app(
        plugin_with(&t, slow_config()).command("/slowfail", move |_ctx, _cmd| {
            let g = g.clone();
            async move {
                g.wait().await;
                Err::<CommandResponse, HandlerError>("late failure".into())
            }
        }),
    );
    let r = within(post_form(
        &client,
        "/slack/commands",
        &command_form("/slowfail", ""),
    ))
    .await;
    r.assert_status(200);
    assert_eq!(r.text(), "");
    gate.open();
    wait_until(Duration::from_secs(5), &|| {
        !t.requests_to(RESPONSE_URL).is_empty()
    })
    .await;
    let v = t.requests_to(RESPONSE_URL)[0].json();
    assert_eq!(v["response_type"], "ephemeral");
    assert!(v["text"].as_str().unwrap().starts_with("Sorry"));
}

#[tokio::test(flavor = "multi_thread")]
async fn event_handler_panic_does_not_break_next_request() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let client = app(
        plugin(&t).on_event("message", move |_ctx, ev: EventCallback| {
            let tx = tx.clone();
            async move {
                assert!(ev.event_id != "EvPanic", "boom");
                tx.send(ev.event_id).unwrap();
                Ok(())
            }
        }),
    );
    post_event(
        &client,
        &event_callback("EvPanic", &json!({"type": "message"})),
    )
    .await
    .assert_status(200);
    post_event(
        &client,
        &event_callback("EvOk", &json!({"type": "message"})),
    )
    .await
    .assert_status(200);
    assert_eq!(recv(&mut rx).await, "EvOk");
}

// ------------------------------------------------------------ interactivity (AC5)

fn block_actions(action_id: &str) -> Value {
    json!({
        "type": "block_actions",
        "user": {"id": "U1", "username": "ada"},
        "team": {"id": "T1"},
        "channel": {"id": "C1"},
        "trigger_id": "trig.2",
        "response_url": RESPONSE_URL,
        "container": {"type": "message", "message_ts": "1.2"},
        "message": {"ts": "1.2", "text": "Approve?"},
        "actions": [{"type": "button", "action_id": action_id, "block_id": "b1", "value": "yes", "action_ts": "1.3"}],
    })
}

fn view_payload(kind: &str, callback_id: &str) -> Value {
    json!({
        "type": kind,
        "user": {"id": "U1"},
        "team": {"id": "T1"},
        "trigger_id": "trig.3",
        "view": {
            "id": "V1",
            "callback_id": callback_id,
            "state": {"values": {"b1": {"name": {"type": "plain_text_input", "value": "ada"}}}},
        },
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn block_action_runs_handler_and_replies_to_response_url() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<BlockAction>();
    let client = app(plugin(&t).action("approve", move |_ctx, a: BlockAction| {
        let tx = tx.clone();
        async move {
            tx.send(a).unwrap();
            Ok(Some(Message::text("Approved").replace_original()))
        }
    }));
    let r = post_interaction(&client, &block_actions("approve")).await;
    r.assert_status(200);
    let a = recv(&mut rx).await;
    assert_eq!(a.action_id, "approve");
    assert_eq!(a.value.as_deref(), Some("yes"));
    assert_eq!(a.block_id.as_deref(), Some("b1"));
    assert_eq!(a.user_id.as_deref(), Some("U1"));
    assert_eq!(a.channel_id.as_deref(), Some("C1"));
    assert_eq!(a.message_ts.as_deref(), Some("1.2"));
    assert_eq!(a.trigger_id.as_deref(), Some("trig.2"));
    wait_until(Duration::from_secs(5), &|| {
        !t.requests_to(RESPONSE_URL).is_empty()
    })
    .await;
    let v = t.requests_to(RESPONSE_URL)[0].json();
    assert_eq!(v["text"], "Approved");
    assert_eq!(v["replace_original"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn view_submission_replies_with_response_action() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t)
        .view_submission("signup", |_ctx, v: ViewPayload| async move {
            let name = v.state_values().unwrap()["b1"]["name"]["value"]
                .as_str()
                .unwrap()
                .to_owned();
            if name == "ada" {
                Ok(ViewResponse::errors([("b1", "Name is taken")]))
            } else {
                Ok(ViewResponse::Clear)
            }
        })
        .view_submission("wizard", |_ctx, _v| async move {
            Ok(ViewResponse::Push(
                json!({"type": "modal", "title": {"type": "plain_text", "text": "Step 2"}}),
            ))
        })
        .view_submission("edit", |_ctx, _v| async move {
            Ok(ViewResponse::Update(json!({"type": "modal"})))
        })
        .view_submission("done", |_ctx, _v| async move { Ok(ViewResponse::Close) }));
    let v: Value = post_interaction(&client, &view_payload("view_submission", "signup"))
        .await
        .json();
    assert_eq!(v["response_action"], "errors");
    assert_eq!(v["errors"]["b1"], "Name is taken");
    let v: Value = post_interaction(&client, &view_payload("view_submission", "wizard"))
        .await
        .json();
    assert_eq!(v["response_action"], "push");
    assert_eq!(v["view"]["title"]["text"], "Step 2");
    let v: Value = post_interaction(&client, &view_payload("view_submission", "edit"))
        .await
        .json();
    assert_eq!(v["response_action"], "update");
    let r = post_interaction(&client, &view_payload("view_submission", "done")).await;
    r.assert_status(200);
    assert_eq!(r.text(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn view_clear_reply() {
    let t = MemoryTransport::new();
    let client =
        app(plugin(&t).view_submission("x", |_ctx, _v| async move { Ok(ViewResponse::Clear) }));
    let v: Value = post_interaction(&client, &view_payload("view_submission", "x"))
        .await
        .json();
    assert_eq!(v["response_action"], "clear");
}

#[tokio::test(flavor = "multi_thread")]
async fn view_closed_and_shortcuts_run_handlers() {
    let t = MemoryTransport::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let (tx2, tx3) = (tx.clone(), tx.clone());
    let client = app(plugin(&t)
        .view_closed("signup", move |_ctx, v: ViewPayload| {
            let tx = tx.clone();
            async move {
                tx.send(format!("closed:{}", v.callback_id)).unwrap();
                Ok(())
            }
        })
        .shortcut("new_ticket", move |_ctx, s: Shortcut| {
            let tx = tx2.clone();
            async move {
                assert_eq!(s.kind, ShortcutKind::Global);
                tx.send(format!("global:{}", s.trigger_id)).unwrap();
                Ok(())
            }
        })
        .shortcut("save_msg", move |_ctx, s: Shortcut| {
            let tx = tx3.clone();
            async move {
                assert_eq!(s.kind, ShortcutKind::Message);
                tx.send(format!("message:{}", s.message.unwrap()["ts"]))
                    .unwrap();
                Ok(())
            }
        }));
    post_interaction(&client, &view_payload("view_closed", "signup"))
        .await
        .assert_status(200);
    assert_eq!(recv(&mut rx).await, "closed:signup");
    post_interaction(
        &client,
        &json!({"type": "shortcut", "callback_id": "new_ticket", "trigger_id": "trig.4", "user": {"id": "U1"}, "team": {"id": "T1"}}),
    )
    .await
    .assert_status(200);
    assert_eq!(recv(&mut rx).await, "global:trig.4");
    post_interaction(
        &client,
        &json!({"type": "message_action", "callback_id": "save_msg", "trigger_id": "trig.5", "user": {"id": "U1"}, "channel": {"id": "C1"}, "message": {"ts": "9.9"}, "response_url": RESPONSE_URL}),
    )
    .await
    .assert_status(200);
    assert_eq!(recv(&mut rx).await, "message:\"9.9\"");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_interaction_and_bad_payload() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t));
    post_interaction(&client, &block_actions("nobody"))
        .await
        .assert_status(200);
    post_interaction(&client, &json!({"type": "block_suggestion"}))
        .await
        .assert_status(200);
    post_form(&client, "/slack/interactions", &[("payload", "not json")])
        .await
        .assert_status(400);
    post_form(&client, "/slack/interactions", &[("other", "x")])
        .await
        .assert_status(400);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_view_submission_acks_empty() {
    let t = MemoryTransport::new();
    let gate = Gate::default();
    let g = gate.clone();
    let client = app(
        plugin_with(&t, slow_config()).view_submission("slow", move |_ctx, _v| {
            let g = g.clone();
            async move {
                g.wait().await;
                Ok(ViewResponse::Clear)
            }
        }),
    );
    let r = within(post_interaction(
        &client,
        &view_payload("view_submission", "slow"),
    ))
    .await;
    r.assert_status(200);
    assert_eq!(r.text(), "", "a late view reply is lost");
    gate.open();
}

#[tokio::test(flavor = "multi_thread")]
async fn view_submission_error_closes_modal_without_leak() {
    let t = MemoryTransport::new();
    let client = app(plugin(&t).view_submission("bad", |_ctx, _v| async move {
        Err::<ViewResponse, HandlerError>("secret detail".into())
    }));
    let r = post_interaction(&client, &view_payload("view_submission", "bad")).await;
    r.assert_status(200);
    assert!(!r.text().contains("secret detail"));
}
