# CLAUDE.md

Slack plugin for autumn-web 0.8. Style for all docs and comments: ASD-STE100.

## Layout

| Path | Contents |
|---|---|
| `src/policy.rs` | Verified core: timestamp window, backoff, retry decision. Pure. |
| `verus/policy.rs` | Verus spec and proofs for `src/policy.rs`. Make the same change in both files. |
| `src/verify.rs` | `v0` signature check. |
| `src/config.rs` | `[slack]` config, profile file, `AUTUMN_SLACK__*` env overlay. |
| `src/transport.rs` | `HttpTransport` trait, `ReqwestTransport`, `MemoryTransport` (fake). |
| `src/client.rs` | `SlackClient`: Web API calls and `response_url` replies. |
| `src/payload.rs` | Typed Slack payloads and replies. |
| `src/handlers.rs` | Handler types, registry, task runner. |
| `src/engine.rs` | Request processing: verify, parse, dedup, dispatch, ack. |
| `src/routes.rs` | Axum and autumn routes. |
| `src/health.rs`, `src/metrics.rs` | Actuator health and Prometheus metrics. |
| `src/plugin.rs` | `SlackPlugin`, `SlackRuntime`, startup checks. |
| `src/testing.rs` | Request signing for app tests. |
| `tests/` | `inbound.rs` (TestApp), `client.rs` (fake), `http.rs` (local fake Slack server), `runtime.rs`, `load.rs`, `boot.rs` (real app boot), `hardening.rs` (review regressions). |
| `docs/` | `plan.md`, `ac-evidence.md`, ADRs. |

## Commands

```bash
git config core.hooksPath .githooks   # pre-commit: fmt, clippy, test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo llvm-cov --all-targets --summary-only
verus verus/policy.rs
```

## Rules

- Write the test first. See it fail. Then write the code.
- A change to `src/policy.rs` needs the same change in `verus/policy.rs`. Run Verus. The test `verus_bodies_match` compares the two.
- Only `src/transport.rs` does network I/O. Other code uses `HttpTransport`.
- Check the signature on the raw bytes before any parse. Only `ssl_check` comes first: its reply is a fixed empty 200.
- Ignore an empty secret. HMAC accepts an empty key.
- Never retry a write call after a 5xx or a network error. It can post twice.
- Never send the bot token to a `response_url`.
- A metrics label is a fixed value or a method name from code. Never Slack data.
- Slack never sees error details. Use `error_text`.
- No `unwrap` or `expect` in `src/` outside tests.
- No `unsafe`.
- A test that needs env vars runs a child process. See `tests/boot.rs`.
- Do mutation tests on a `git clone` in the scratchpad. Never in the repo.
- Delivery is at least once. Do not claim exactly once.
- Do not log bodies, tokens, or secrets.
