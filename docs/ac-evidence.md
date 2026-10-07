# Acceptance criteria: evidence

No GitHub issue exists for this work. The criteria come from `docs/plan.md`
section 8. Tests are named `file::test`. Unit tests are in `src/<module>.rs`.

| AC | Criterion | Evidence |
|---|---|---|
| AC1 | `SlackPlugin` implements `Plugin`. One call installs it. Contract for autumn-web 0.8. `[slack]` section. | `src/plugin.rs` (`impl Plugin for SlackPlugin`). Tests: `runtime::plugin_installs_with_one_call` (TestApp; `/actuator/health` shows `slack` UP), `runtime::plugin_name_contract_and_config_section`, `runtime::plugin_declares_its_routes` (POST routes, `Public`, source = plugin), `boot::slack_only_app_boots_and_serves_signed_requests` (real `run()`, no other routes), `boot::…` also checks that the client exists in a state initializer, before job workers start. Manual: `examples/app.rs` with `curl` (challenge 200, `/deploy` Block Kit reply, forged 401). |
| AC2 | `v0` HMAC on raw body, constant-time, timestamp tolerance, rotation. 400 for bad input, 401 for a failed check. No handler before the check. | `src/verify.rs`, `src/policy.rs::timestamp_fresh`. Tests: `verify::slack_doc_example_verifies` (Slack's published vector), `verify::errors_in_order`, `verify::any_signed_body_verifies_and_any_flip_fails` (proptest), `verify::empty_secrets_never_verify`, `inbound::missing_headers_give_400`, `inbound::malformed_signature_gives_400`, `inbound::tampered_body_gives_401`, `inbound::stale_timestamp_gives_401_and_tolerance_edge_passes`, `inbound::previous_secret_verifies_during_rotation`, `hardening::every_route_rejects_bad_signatures_before_any_handler` (all 3 routes), `hardening::empty_previous_secret_does_not_let_anyone_sign`, `boot::…` (old secret from env var). Verus: `timestamp_fresh` plus 3 lemmas. |
| AC3 | Events: `url_verification`, dispatch by type, ack before the handler ends, dedup by `event_id`, unknown events 200. | `src/engine.rs::events`. Tests: `inbound::url_verification_echoes_challenge`, `inbound::event_callback_runs_typed_handler_after_ack`, `inbound::ack_does_not_wait_for_slow_event_handler` (gate, no timing), `inbound::retried_event_id_runs_handler_once`, `hardening::retry_headers_reach_the_handler_and_dedup_counts`, `inbound::unknown_and_rate_limited_callbacks_get_200`, `inbound::bad_event_json_gives_400`. |
| AC4 | Commands: typed `SlashCommand`, ephemeral / in-channel / empty ack, late reply to `response_url`, unknown command reply. | `src/engine.rs::commands`, `ack_or_late`. Tests: `inbound::command_replies_in_time_with_ephemeral_message`, `inbound::command_in_channel_with_blocks_and_empty_ack`, `inbound::slow_command_acks_then_posts_to_response_url`, `inbound::late_command_error_posts_error_text`, `hardening::late_ack_counts_once_and_late_ack_reply_posts_nothing`, `inbound::unknown_command_gets_ephemeral_text`, `inbound::ssl_check_gets_200`, `hardening::ssl_check_needs_no_signature`, `inbound::bad_command_form_gives_400`. |
| AC5 | Interactivity: `block_actions`, `view_submission` (`clear`/`update`/`push`/`errors`), `view_closed`, `shortcut`, `message_action`; action replies to `response_url`. | `src/payload.rs::Interaction`, `src/engine.rs::interactions`. Tests: `inbound::block_action_runs_handler_and_replies_to_response_url`, `inbound::view_submission_replies_with_response_action`, `inbound::view_clear_reply`, `inbound::view_closed_and_shortcuts_run_handlers`, `inbound::slow_view_submission_acks_empty`, `inbound::view_submission_error_closes_modal_without_leak`, `inbound::unknown_interaction_and_bad_payload`, `hardening::block_suggestion_gets_no_options`, `payload::interaction_parse_kinds` (incl. org-wide `team: null`). |
| AC6 | `SlackClient`: any method plus typed calls; `ok: false` typed error; pagination. | `src/client.rs`. Tests: `client::post_message_sends_form_with_bearer_token`, `client::typed_calls_hit_their_methods` (all 11 typed methods), `client::auth_test_is_typed`, `client::ok_false_gives_typed_api_error`, `client::generic_call_and_bad_params`, `client::non_json_and_http_errors`, `client::paginate_follows_next_cursor`, `client::paginate_stops_at_max_pages`, `client::missing_token_gives_config_error`, `hardening::api_calls_drop_reply_only_fields`, `http::real_http_round_trip_with_429_retry` (real HTTP). |
| AC7 | Retry: 429 waits `Retry-After` (capped); 5xx and network errors retry only for reads; bounded attempts. | `src/policy.rs::decide` (Verus-proven), `src/client.rs::call_with`. Tests: `client::rate_limit_waits_retry_after_then_succeeds`, `client::rate_limit_over_cap_gives_rate_limited_error`, `client::rate_limit_attempts_are_bounded`, `client::server_error_retries_read_but_not_write`, `client::network_error_retries_read_only`, `client::call_read_marks_custom_method_idempotent`, `hardening::read_call_stops_after_max_attempts_on_5xx`, `hardening::trigger_calls_do_not_wait_past_the_trigger_life`, `policy::decide_invariants` and `policy::backoff_matches_closed_form` (proptests). |
| AC8 | `response_url` only to an allowed host over HTTPS. | `src/client.rs::allowed_response_url`. Tests: `client::respond_posts_json_to_allowed_host_only` (7 bad URLs incl. userinfo, port, look-alike host, metadata IP), `client::respond_reports_http_failure`, `src/client.rs::response_url_rules`, `src/client.rs::only_listed_hosts_pass` (proptest). Token never sent: `inbound::slow_command_acks_then_posts_to_response_url`. |
| AC9 | Handler errors and panics do not crash the app. Drain at shutdown. | `src/handlers.rs::Runner` (`catch_unwind`, `InFlight` guard), `src/plugin.rs::drain`. Tests: `inbound::command_error_and_panic_give_error_text_not_500`, `inbound::event_handler_panic_does_not_break_next_request`, `hardening::handler_panic_and_error_are_counted`, `runtime::shutdown_waits_for_in_flight_handlers`, `hardening::shutdown_returns_only_after_the_handler_ends`, `runtime::shutdown_gives_up_after_drain_timeout`, `hardening::routes_reply_503_after_shutdown_and_keep_retries`, `plugin::shutdown_hook_drains_the_started_engine`. |
| AC10 | Health (`auth.test`, cached, opt-in readiness). Metrics with fixed label sets. | `src/health.rs`, `src/metrics.rs`. Tests: `runtime::health_unknown_before_start_and_without_token`, `runtime::health_up_down_cached_and_no_secrets`, `runtime::health_cache_expires`, `hardening::health_check_is_single_flight_and_one_attempt`, `hardening::single_flight_holds_with_zero_cache_time`, `hardening::down_result_clears_fast`, `hardening::late_ack_counts_while_the_handler_still_runs`, `hardening::ssl_check_has_its_own_label`, `health::error_codes_are_short`, `runtime::metrics_count_requests_handlers_and_api_calls` (incl. `/actuator/prometheus`), `runtime::metric_labels_never_hold_slack_data`, `metrics::counts_and_families`. |
| AC11 | Serde config, validation, layered TOML, `AUTUMN_SLACK__*`, secrets from env vars. Startup fails on a missing secret or a CSRF/CAPTCHA block. | `src/config.rs`, `src/plugin.rs::check_security`. Tests: `config::*` (6), `load::loads_base_profile_file_and_env_in_order`, `load::no_files_gives_defaults`, `load::bad_env_value_names_the_variable`, `runtime::missing_signing_secret_fails_start`, `runtime::invalid_config_fails_start`, `runtime::duplicate_or_bad_handlers_fail_start`, `runtime::csrf_without_exemption_fails_start_with_fix`, `runtime::captcha_without_exemption_fails_start`, `hardening::captcha_dev_bypass_and_json_events_pass`, `hardening::root_mount_csrf_fix_names_exact_paths`, `hardening::worker_role_needs_no_secret_and_no_exemption`, `hardening::failed_startup_stops_the_app_with_the_reason`, `hardening::plain_http_api_base_url_needs_loopback`, `plugin::exempt_rule_matches_autumn`. |
| AC12 | `testing::sign` and `MemoryTransport` are public. | `src/testing.rs` (`sign`, `signed_headers`), `src/transport.rs` (`MemoryTransport`, `HttpReply`, `set_latency`). All integration tests use them through the public API. Tests: `testing::signed_headers_verify`, `transport::memory_default_seq_and_failures`. |
| AC13 | fmt, clippy pedantic + nursery, no `unwrap` in `src/`, tests, coverage 85% or more, CI. | CI (`.github/workflows/ci.yml`): fmt, clippy `-D warnings`, `cargo llvm-cov --fail-under-lines 85`, Verus. Green on autumn-foundation/autumn-plugin-slack#1; runs on autumn-foundation/autumn-plugin-slack#2. Local: 0 clippy warnings; 141 tests pass (2 ignored: the boot child process and one doc example); line coverage 94.84%, region coverage 93.87%. `unwrap_used`/`expect_used` warn for non-test code. Pre-commit hook: `.githooks/pre-commit`. |
| AC14 | Verus specs state the policy invariants. Proofs pass. | `verus/policy.rs`: 18 verified, 0 errors (`timestamp_fresh`, `backoff_ms` with closed form, cap and zero lemmas, `decide`, `retry_after_ms`). Spec mutants rejected: 3 of 3 in round 0, 6 of 6 in round 1. `policy::verus_bodies_match` fails when `src/policy.rs` drifts from the spec. CI job `verus` builds Verus at a pinned commit. |
| AC15 | README, CLAUDE.md, ADRs, Mermaid, ASD-STE100. | `README.md` (Mermaid flow), `CLAUDE.md`, `docs/plan.md` (Mermaid design), `docs/adr/0001-own-signature-check-and-routes.md`, `docs/adr/0002-own-transport-and-verified-retry.md`. A docs reviewer checked STE; about 30 rewrites applied. |

## TDD record

- SPEC and PROOF first: `verus/policy.rs` verified before any Rust code.
- RED: 102 tests failed with `not yet implemented`. Only 6 structural tests passed.
- GREEN: all tests passed. Then REFACTOR with tests green.
- A real app run found a bug the tests missed (autumn stops with no `Route`).
  The regression test `boot::…` failed first, then passed after the fix.

## Review rounds

Review limit: 2 to 3 rounds. Round 2 found no high finding, so the review
stopped after round 2.

### Round 1: six reviewers

| Angle | Findings | Result |
|---|---|---|
| Security | 1 high, 1 medium, 1 low | All fixed. High: an empty previous signing secret let anyone sign. |
| Slack protocol | 6 low | All fixed (`ssl_check` order, `block_suggestion`, org-wide `team_id`, ack cap 2700 ms, trigger wait cap, README `thread_ts`), plus `retry_reason`. |
| autumn integration | 3 medium, 4 low | All fixed (strict config, worker role, early jobs, root CSRF text, empty exemption, CAPTCHA rules, drain bound in docs). |
| Concurrency | 1 medium, 7 low | 7 fixed. Not fixed: a limit on concurrent handlers (out of scope, in README). |
| Tests and spec | 12 | All fixed. 8 code mutants and 6 spec mutants that survived now fail. |
| Docs and STE | 11 accuracy, about 30 style | All fixed. |

### Round 2: two reviewers on the round-1 diff

| Angle | Findings | Result |
|---|---|---|
| Code | 1 medium, 4 low | 4 fixed (single flight with `cache_secs = 0`, late-ack count time, `ssl_check` memory, start error log). Not fixed: a reply lost when the client drops the request at the same moment; that reply cannot reach the client in any case. |
| Tests, spec, docs | 8 | 7 fixed. Not fixed: mutant G4 (a request gone before the reply is counted as late) needs a dropped-request harness; mutant I2 (the "did not start" branch) guards a path that autumn does not take. |

### Mutation checks after the fixes

| Mutant | Result |
|---|---|
| Empty secret kept | Killed |
| No signature check on `/commands` or `/interactions` | Killed |
| No 503 after shutdown | Killed |
| Reply-only fields sent to the Web API | Killed |
| No trigger wait cap / three trigger attempts | Killed |
| Health retries / no single flight / single flight off with `cache_secs = 0` | Killed |
| Late ack counted at handler end / in-time ack counted late | Killed |
| `ssl_check` counted as `ok` | Killed |
| Plugin starts in `on_startup` (no client for early jobs) | Killed (real boot test) |
| `src/policy.rs` drifts from the Verus spec | Killed (`policy::verus_bodies_match`) |
| `backoff_ms` doubling changed | Killed (`policy::backoff_matches_closed_form`) |
| Verus: six weaker `decide` variants | Rejected by Verus |
