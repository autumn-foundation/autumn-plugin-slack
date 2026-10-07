# ADR 0002: Own transport and a verified retry policy

- Status: Accepted
- Date: 2026-10-07

## Context

All Slack Web API calls are HTTP `POST`. Slack replies 429 with
`Retry-After` when an app sends too many calls. A 429 means Slack did not
do the call. After a 5xx or a connection failure, Slack possibly did the call.

autumn-web 0.8 has `http_client::Client`. By default it retries only
idempotent HTTP methods. It does not retry `POST`.

## Options

1. Use autumn `Client` and turn on `POST` retries.
2. Write a small `HttpTransport` trait. Put the retry rules in a pure,
   verified function (`policy::decide`).

## Decision

Option 2.

## Reasons

- Option 1 retries a `POST` after a 5xx. `chat.postMessage` can then post
  twice.
- The right rule depends on the Slack method, not the HTTP method: retry 429
  for all calls; retry 5xx and network errors only for read calls.
- A pure function is easy to prove. Verus proves: bounded attempts, bounded
  waits, no 5xx retry for writes, and the exact `Retry-After` wait.
- The trait gives `MemoryTransport` for tests with no network.

## Results

- Good: no double posts from retries.
- Good: app tests use `MemoryTransport`. `tests/http.rs` tests the real
  transport against a local server.
- Bad: autumn `[http_client]` config and HTTP interceptors do not apply to
  Slack calls.

## Ack strategy

Slack must get a reply in 3 s.

- Events, actions, view closes, shortcuts: the reply has no content. The
  plugin acks at once and runs the handler in the background.
- Commands and view submissions: the reply has content. The plugin waits up
  to `ack_timeout_ms`. A late command reply goes to `response_url`. Slack
  does not get a late view reply. Slack closes the view.
- One tracked task owns the ack wait. If the request is gone, that task
  still sends the late reply.
- The plugin tracks background handlers. At shutdown, the routes reply 503
  and the plugin waits for handlers up to `drain_timeout_secs`. A restart
  stops background handlers: use `#[job]` for durable work.

## Retry budget

The longest call is about `max_attempts × timeout_ms + (max_attempts − 1) ×
max_wait_ms` (default about 90 s). This is longer than the drain. Shutdown
can stop a call that waits for a retry. Calls with a `trigger_id` wait 1 s
or less, because the trigger is valid for 3 s only. The health check makes
one attempt.
