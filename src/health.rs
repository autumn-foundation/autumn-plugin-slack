//! Health indicator for `/actuator/health`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use autumn_web::actuator::{HealthCheckOutput, HealthIndicator, HealthStatus, IndicatorGroup};
use futures::future::BoxFuture;
use serde_json::json;
use tokio::time::Instant;

use crate::client::SlackClient;
use crate::error::SlackError;

/// Time limit for one check. 1.5 s is less than the 2 s autumn indicator
/// timeout.
const CHECK_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Largest time to keep a `Down` result. A short outage clears fast.
pub const DOWN_TTL: Duration = Duration::from_secs(5);

/// A stored result and its expiry.
#[derive(Clone)]
struct Entry {
    stored: Instant,
    until: Instant,
    out: HealthCheckOutput,
}

/// Checks the bot token with `auth.test`.
///
/// - `Up`: Slack accepts the token.
/// - `Down`: Slack rejects it, or the call fails.
/// - `Unknown`: before start, or with no bot token.
///
/// It keeps `Up` for `[slack.health] cache_secs` and `Down` for 5 s or less.
/// One check runs at a time; other probes get its result. Details give a
/// short error code only.
pub struct SlackHealth {
    client: OnceLock<SlackClient>,
    readiness: bool,
    cache: Mutex<Option<Entry>>,
    cache_for: OnceLock<Duration>,
    /// Lets one check run at a time.
    refresh: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for SlackHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackHealth")
            .field("started", &self.client.get().is_some())
            .field("readiness", &self.readiness)
            .finish_non_exhaustive()
    }
}

impl SlackHealth {
    pub(crate) const fn new(readiness: bool) -> Self {
        Self {
            client: OnceLock::new(),
            readiness,
            cache: Mutex::new(None),
            cache_for: OnceLock::new(),
            refresh: tokio::sync::Mutex::const_new(()),
        }
    }

    /// Sets the client once. Later calls do nothing.
    pub(crate) fn set(&self, client: SlackClient, cache_for: Duration) {
        let _ = self.cache_for.set(cache_for);
        let _ = self.client.set(client);
    }

    fn entry(&self) -> Option<Entry> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn cached(&self) -> Option<HealthCheckOutput> {
        let entry = self.entry()?;
        (Instant::now() < entry.until).then_some(entry.out)
    }

    /// A result stored at or after `since`, whatever its TTL. A probe that
    /// waited for another check uses that check's result.
    fn stored_since(&self, since: Instant) -> Option<HealthCheckOutput> {
        let entry = self.entry()?;
        (entry.stored >= since).then_some(entry.out)
    }

    fn store(&self, out: &HealthCheckOutput) {
        let up_ttl = self.cache_for.get().copied().unwrap_or_default();
        let ttl = if out.status == HealthStatus::Up {
            up_ttl
        } else {
            up_ttl.min(DOWN_TTL)
        };
        *self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Entry {
            stored: Instant::now(),
            until: Instant::now() + ttl,
            out: out.clone(),
        });
    }

    async fn run_check(client: &SlackClient) -> HealthCheckOutput {
        let down = |code: String| {
            HealthCheckOutput::down()
                .with_details(HashMap::from([("error".to_owned(), json!(code))]))
        };
        match tokio::time::timeout(CHECK_TIMEOUT, client.auth_test_once()).await {
            Ok(Ok(())) => HealthCheckOutput::up(),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "slack health check failed");
                down(error_code(&e))
            }
            Err(_) => down("timeout".to_owned()),
        }
    }
}

/// A short error code for public health output.
fn error_code(err: &SlackError) -> String {
    match err {
        // Slack codes are short snake_case words. Keep only those characters.
        SlackError::Api { code, .. } => code
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect(),
        SlackError::RateLimited { .. } => "rate_limited".to_owned(),
        SlackError::Http { status, .. } => format!("http_{status}"),
        SlackError::Transport(_) => "network".to_owned(),
        _ => "error".to_owned(),
    }
}

fn unknown(reason: &str) -> HealthCheckOutput {
    HealthCheckOutput {
        status: HealthStatus::Unknown,
        details: HashMap::from([("reason".to_owned(), json!(reason))]),
    }
}

impl HealthIndicator for SlackHealth {
    fn check(&self) -> BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move {
            let Some(client) = self.client.get() else {
                return unknown("not started");
            };
            if !client.has_token() {
                return unknown("no bot token");
            }
            if let Some(out) = self.cached() {
                return out;
            }
            // Single flight: a probe that waits here gets the new result,
            // also with `cache_secs = 0`.
            let since = Instant::now();
            let _turn = self.refresh.lock().await;
            if let Some(out) = self.cached().or_else(|| self.stored_since(since)) {
                return out;
            }
            let out = Self::run_check(client).await;
            self.store(&out);
            out
        })
    }

    fn group(&self) -> IndicatorGroup {
        if self.readiness {
            IndicatorGroup::Readiness
        } else {
            IndicatorGroup::HealthOnly
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_before_start() {
        let h = SlackHealth::new(false);
        let out = h.check().await;
        assert_eq!(out.status, HealthStatus::Unknown);
        assert_eq!(out.details["reason"], json!("not started"));
        assert!(format!("{h:?}").contains("started: false"));
    }

    #[test]
    fn error_codes_are_short() {
        assert_eq!(
            error_code(&SlackError::Api {
                method: "auth.test".into(),
                code: "invalid_auth".into(),
                needed: None
            }),
            "invalid_auth"
        );
        assert_eq!(
            error_code(&SlackError::RateLimited {
                retry_after_secs: None
            }),
            "rate_limited"
        );
        assert_eq!(
            error_code(&SlackError::Http {
                status: 503,
                body: "x".into()
            }),
            "http_503"
        );
        assert_eq!(error_code(&SlackError::Transport("dns".into())), "network");
        assert_eq!(error_code(&SlackError::Decode("x".into())), "error");
    }
}
