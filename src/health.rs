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

/// Checks the bot token with `auth.test`.
///
/// - `Up`: Slack accepts the token.
/// - `Down`: Slack rejects it, or the call fails.
/// - `Unknown`: before start, or with no bot token.
///
/// It keeps a result for `[slack.health] cache_secs`. Details give a short
/// error code only.
pub struct SlackHealth {
    client: OnceLock<SlackClient>,
    readiness: bool,
    cache: Mutex<Option<(Instant, HealthCheckOutput)>>,
    cache_for: OnceLock<Duration>,
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
        }
    }

    /// Sets the client once. Later calls do nothing.
    pub(crate) fn set(&self, client: SlackClient, cache_for: Duration) {
        let _ = self.cache_for.set(cache_for);
        let _ = self.client.set(client);
    }

    fn cached(&self) -> Option<HealthCheckOutput> {
        let ttl = *self.cache_for.get()?;
        let (at, out) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        (at.elapsed() < ttl).then_some(out)
    }

    fn store(&self, out: &HealthCheckOutput) {
        *self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Instant::now(), out.clone()));
    }
}

/// Time limit for one check.
const CHECK_TIMEOUT: Duration = Duration::from_millis(1_500);

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
            // Under the 2 s indicator timeout.
            let result = tokio::time::timeout(CHECK_TIMEOUT, client.auth_test()).await;
            let out = match result {
                Ok(Ok(_)) => HealthCheckOutput::up(),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "slack health check failed");
                    HealthCheckOutput::down()
                        .with_details(HashMap::from([("error".to_owned(), json!(error_code(&e)))]))
                }
                Err(_) => HealthCheckOutput::down()
                    .with_details(HashMap::from([("error".to_owned(), json!("timeout"))])),
            };
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
