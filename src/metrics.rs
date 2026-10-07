//! Counters and gauges for `/actuator/prometheus`.
//!
//! Label values come from fixed sets, or from Web API method names in app
//! code. They never come from Slack data.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_web::actuator::{MetricFamily, MetricKind, MetricSample, MetricsSource};

/// An inbound route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Surface {
    Events,
    Commands,
    Interactions,
}

impl Surface {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Commands => "commands",
            Self::Interactions => "interactions",
        }
    }
}

type Pair = (&'static str, &'static str);

/// Plugin metrics. Also a [`MetricsSource`].
#[derive(Debug, Default)]
pub struct SlackMetrics {
    requests: Mutex<BTreeMap<Pair, u64>>,
    handlers: Mutex<BTreeMap<Pair, u64>>,
    late: Mutex<BTreeMap<&'static str, u64>>,
    api: Mutex<BTreeMap<(String, &'static str), u64>>,
    retries: Mutex<BTreeMap<String, u64>>,
    in_flight: AtomicU64,
}

impl SlackMetrics {
    /// Makes empty metrics.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn request(&self, surface: Surface, outcome: &'static str) {
        *lock(&self.requests)
            .entry((surface.as_str(), outcome))
            .or_default() += 1;
    }

    pub(crate) fn handler(&self, surface: Surface, outcome: &'static str) {
        *lock(&self.handlers)
            .entry((surface.as_str(), outcome))
            .or_default() += 1;
    }

    /// A handler that missed the ack. Its run also counts in `handler`.
    pub(crate) fn late_ack(&self, surface: Surface) {
        *lock(&self.late).entry(surface.as_str()).or_default() += 1;
    }

    pub(crate) fn api_call(&self, method: &str, outcome: &'static str) {
        *lock(&self.api)
            .entry((method.to_owned(), outcome))
            .or_default() += 1;
    }

    pub(crate) fn api_retry(&self, method: &str) {
        *lock(&self.retries).entry(method.to_owned()).or_default() += 1;
    }

    pub(crate) fn in_flight_add(&self, delta: i64) {
        let step = delta.unsigned_abs();
        // Saturate at 0: an extra decrement must not wrap below 0.
        let _ = self
            .in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(if delta >= 0 {
                    v.saturating_add(step)
                } else {
                    v.saturating_sub(step)
                })
            });
    }

    /// Requests by surface (`events`, `commands`, `interactions`) and outcome.
    #[must_use]
    pub fn requests(&self, surface: &str, outcome: &str) -> u64 {
        get(&lock(&self.requests), surface, outcome)
    }

    /// Handler runs by surface and outcome (`ok`, `error`, `panic`). Each run
    /// counts once.
    #[must_use]
    pub fn handler_runs(&self, surface: &str, outcome: &str) -> u64 {
        get(&lock(&self.handlers), surface, outcome)
    }

    /// Handlers that missed the ack, by surface.
    #[must_use]
    pub fn late_acks(&self, surface: &str) -> u64 {
        lock(&self.late).get(surface).copied().unwrap_or(0)
    }

    /// Web API calls by method and outcome.
    #[must_use]
    pub fn api_calls(&self, method: &str, outcome: &str) -> u64 {
        lock(&self.api)
            .iter()
            .find(|((m, o), _)| m == method && *o == outcome)
            .map_or(0, |(_, n)| *n)
    }

    /// Web API retries by method.
    #[must_use]
    pub fn api_retries(&self, method: &str) -> u64 {
        lock(&self.retries).get(method).copied().unwrap_or(0)
    }

    /// Handlers that run now.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::SeqCst)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn get(map: &BTreeMap<Pair, u64>, a: &str, b: &str) -> u64 {
    map.iter()
        .find(|((x, y), _)| *x == a && *y == b)
        .map_or(0, |(_, n)| *n)
}

#[allow(clippy::cast_precision_loss)] // Counters stay far below 2^53.
const fn as_f64(v: u64) -> f64 {
    v as f64
}

fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn family(name: &str, help: &str, kind: MetricKind, samples: Vec<MetricSample>) -> MetricFamily {
    MetricFamily {
        name: name.to_owned(),
        help: help.to_owned(),
        kind,
        samples,
    }
}

impl MetricsSource for SlackMetrics {
    fn collect(&self) -> Vec<MetricFamily> {
        let pairs = |map: &BTreeMap<Pair, u64>, a: &str| -> Vec<MetricSample> {
            map.iter()
                .map(|((x, y), n)| MetricSample {
                    labels: labels(&[(a, x), ("outcome", y)]),
                    value: as_f64(*n),
                })
                .collect()
        };
        let requests = pairs(&lock(&self.requests), "surface");
        let handlers = pairs(&lock(&self.handlers), "surface");
        let api = lock(&self.api)
            .iter()
            .map(|((m, o), n)| MetricSample {
                labels: labels(&[("method", m), ("outcome", o)]),
                value: as_f64(*n),
            })
            .collect();
        let late = lock(&self.late)
            .iter()
            .map(|(s, n)| MetricSample {
                labels: labels(&[("surface", s)]),
                value: as_f64(*n),
            })
            .collect();
        let retries = lock(&self.retries)
            .iter()
            .map(|(m, n)| MetricSample {
                labels: labels(&[("method", m)]),
                value: as_f64(*n),
            })
            .collect();
        vec![
            family(
                "slack_requests_total",
                "Inbound Slack requests by surface and outcome.",
                MetricKind::Counter,
                requests,
            ),
            family(
                "slack_handler_runs_total",
                "Handler runs by surface and outcome.",
                MetricKind::Counter,
                handlers,
            ),
            family(
                "slack_late_acks_total",
                "Handlers that missed the ack, by surface.",
                MetricKind::Counter,
                late,
            ),
            family(
                "slack_api_calls_total",
                "Web API calls by method and outcome.",
                MetricKind::Counter,
                api,
            ),
            family(
                "slack_api_retries_total",
                "Web API retries by method.",
                MetricKind::Counter,
                retries,
            ),
            family(
                "slack_handlers_in_flight",
                "Handlers that run now.",
                MetricKind::Gauge,
                vec![MetricSample {
                    labels: Vec::new(),
                    value: as_f64(self.in_flight()),
                }],
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_families() {
        let m = SlackMetrics::new();
        m.request(Surface::Events, "ok");
        m.request(Surface::Events, "ok");
        m.handler(Surface::Commands, "ok");
        m.late_ack(Surface::Commands);
        m.api_call("chat.postMessage", "ok");
        m.api_retry("chat.postMessage");
        m.in_flight_add(2);
        m.in_flight_add(-1);
        assert_eq!(m.requests("events", "ok"), 2);
        assert_eq!(m.requests("events", "bad_signature"), 0);
        assert_eq!(m.handler_runs("commands", "ok"), 1);
        assert_eq!(m.late_acks("commands"), 1);
        assert_eq!(m.late_acks("events"), 0);
        assert_eq!(m.api_calls("chat.postMessage", "ok"), 1);
        assert_eq!(m.api_retries("chat.postMessage"), 1);
        assert_eq!(m.in_flight(), 1);
        let f = m.collect();
        let req = f.iter().find(|f| f.name == "slack_requests_total").unwrap();
        assert_eq!(req.kind, MetricKind::Counter);
        assert!((req.samples[0].value - 2.0).abs() < f64::EPSILON);
        assert_eq!(
            req.samples[0].labels,
            vec![
                ("surface".to_owned(), "events".to_owned()),
                ("outcome".to_owned(), "ok".to_owned())
            ]
        );
        let g = f
            .iter()
            .find(|f| f.name == "slack_handlers_in_flight")
            .unwrap();
        assert_eq!(g.kind, MetricKind::Gauge);
        assert!((g.samples[0].value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn in_flight_never_wraps_below_zero() {
        let m = SlackMetrics::new();
        m.in_flight_add(-1);
        assert_eq!(m.in_flight(), 0);
    }
}
