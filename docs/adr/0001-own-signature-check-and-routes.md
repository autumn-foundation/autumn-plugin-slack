# ADR 0001: Own signature check and routes, not `SignedWebhook`

- Status: Accepted
- Date: 2026-10-07

## Context

autumn-web 0.8 has `SignedWebhook` with a `slack` preset. It checks the `v0`
signature and the timestamp. It also needs a replay ID: the JSON `event_id`,
or the `challenge`. With no replay ID, it replies 400.

Slack sends three kinds of requests:

1. Events API: JSON. It has `event_id`.
2. Slash commands: a form. No delivery ID.
3. Interactivity: a form with a JSON `payload` field. No delivery ID.

## Options

1. Use `SignedWebhook` for events. Write a second check for commands and
   interactions.
2. Write one check for all three. Reuse autumn's `WebhookReplayStore` for the
   event dedup.
3. Ask autumn to accept "no replay ID" in `SignedWebhook`. Wait for it.

## Decision

Option 2.

## Reasons

- One check, one error contract, and one set of tests for all routes.
- `SignedWebhook` replies 409 to a repeat. Slack then retries again. The
  plugin replies 200 to a repeat, so Slack stops.
- `SignedWebhook` needs config in `[security.webhooks]`. The plugin needs only
  `[slack]`.
- `WebhookReplayStore` gives memory and Redis stores. The plugin reuses it.
- The timestamp rule is in the verified policy core (`timestamp_fresh`).

## Results

- Good: all three routes are verified the same way, before any parse.
- Good: secret rotation works on all routes.
- Bad: two signature checks exist in an app that also uses `SignedWebhook`.

## Related decisions

- **Routes are autumn `Route` values**, not a nested router. autumn stops at
  boot when an app has no `Route`. A Slack-only bot has none. `Route` also
  shows in `autumn routes`. A `Route` path is `&'static str`, so the plugin
  leaks three short strings per app build.
- **The base path is a builder setting.** autumn mounts routes before it
  loads config.
- **CSRF and CAPTCHA:** a plugin cannot add an exemption. The plugin checks
  at startup and fails with the config fix.

## Upstream seams (proposed)

1. `AppBuilder::csrf_exempt_path(path)` and `captcha_exempt_path(path)` for
   plugins.
2. A `SignedWebhook` option for providers with no replay ID.
