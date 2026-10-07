//! Pure policy core. Verified with Verus in `verus/policy.rs`.
//!
//! Keep this file and the Verus file in step.

/// Slack limit: reject a request older or newer than 5 minutes.
pub const DEFAULT_TOLERANCE_SECS: u64 = 300;
/// Largest number of attempts for one Web API call.
pub const MAX_ATTEMPTS: u32 = 10;
/// Wait for a 429 with no `Retry-After` header.
pub const DEFAULT_RETRY_AFTER_MS: u64 = 1_000;

/// Returns `true` when `ts` is within `tolerance` seconds of `now`.
#[must_use]
pub const fn timestamp_fresh(now: u64, ts: u64, tolerance: u64) -> bool {
    now.abs_diff(ts) <= tolerance
}

/// Returns the wait in ms before retry number `attempt` (1-based):
/// `initial * 2^(attempt - 1)`, capped at `cap`.
#[must_use]
pub const fn backoff_ms(initial: u64, attempt: u32, cap: u64) -> u64 {
    let mut b = if initial <= cap { initial } else { cap };
    let mut i = 1;
    while i < attempt {
        // Compare without overflow: `cap - b <= b` is `2b >= cap`.
        b = if cap - b <= b { cap } else { b * 2 };
        i += 1;
    }
    b
}

/// The class of one Web API attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// HTTP 2xx. The body can still be `ok: false`.
    Success,
    /// HTTP 429.
    RateLimited,
    /// HTTP 5xx.
    ServerError,
    /// No HTTP reply: connect error or timeout.
    NetworkError,
    /// Other HTTP status.
    ClientError,
}

/// What the client does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Return the result as it is.
    Done,
    /// Wait this many ms, then try again.
    Retry(u64),
    /// Stop. Return the last error.
    GiveUp,
}

/// Retry rules for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryRule {
    /// Attempts in total, 1 to [`MAX_ATTEMPTS`].
    pub max_attempts: u32,
    /// First backoff for 5xx and network errors.
    pub initial_backoff_ms: u64,
    /// Largest wait before a retry.
    pub max_wait_ms: u64,
}

/// Decides the next step after attempt number `attempt` (1-based).
///
/// - 429: retry after `Retry-After` while attempts are left and the wait is
///   in the cap.
/// - 5xx and network errors: retry with backoff only when `idempotent`.
/// - Other outcomes: done.
#[must_use]
pub const fn decide(
    outcome: Outcome,
    attempt: u32,
    idempotent: bool,
    retry_after_ms: Option<u64>,
    rule: &RetryRule,
) -> Step {
    match outcome {
        Outcome::Success | Outcome::ClientError => Step::Done,
        Outcome::RateLimited => {
            let wait = match retry_after_ms {
                Some(w) => w,
                None => DEFAULT_RETRY_AFTER_MS,
            };
            if attempt < rule.max_attempts && wait <= rule.max_wait_ms {
                Step::Retry(wait)
            } else {
                Step::GiveUp
            }
        }
        Outcome::ServerError | Outcome::NetworkError => {
            if idempotent && attempt < rule.max_attempts {
                Step::Retry(backoff_ms(
                    rule.initial_backoff_ms,
                    attempt,
                    rule.max_wait_ms,
                ))
            } else {
                Step::GiveUp
            }
        }
    }
}

/// Converts `Retry-After` seconds to ms. It saturates.
#[must_use]
pub const fn retry_after_ms(secs: u64) -> u64 {
    if secs <= u64::MAX / 1000 {
        secs * 1000
    } else {
        u64::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const RULE: RetryRule = RetryRule {
        max_attempts: 3,
        initial_backoff_ms: 100,
        max_wait_ms: 1_000,
    };

    #[test]
    fn verus_constants_match() {
        let spec = include_str!("../verus/policy.rs");
        for line in [
            "pub const DEFAULT_TOLERANCE_SECS: u64 = 300;",
            "pub const MAX_ATTEMPTS: u32 = 10;",
            "pub const DEFAULT_RETRY_AFTER_MS: u64 = 1_000;",
        ] {
            assert!(spec.contains(line), "verus/policy.rs lacks {line}");
        }
    }

    #[test]
    fn fresh_edges() {
        assert!(timestamp_fresh(1_000, 700, 300));
        assert!(!timestamp_fresh(1_000, 699, 300));
        assert!(timestamp_fresh(1_000, 1_300, 300));
        assert!(!timestamp_fresh(1_000, 1_301, 300));
        assert!(timestamp_fresh(u64::MAX, u64::MAX, 0));
        assert!(!timestamp_fresh(0, u64::MAX, u64::MAX - 1));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_ms(100, 0, 1_000), 100);
        assert_eq!(backoff_ms(100, 1, 1_000), 100);
        assert_eq!(backoff_ms(100, 2, 1_000), 200);
        assert_eq!(backoff_ms(100, 4, 1_000), 800);
        assert_eq!(backoff_ms(100, 5, 1_000), 1_000);
        assert_eq!(backoff_ms(5_000, 1, 1_000), 1_000);
        assert_eq!(backoff_ms(u64::MAX, 60, u64::MAX), u64::MAX);
    }

    #[test]
    fn decide_table() {
        use Outcome::*;
        assert_eq!(decide(Success, 1, false, None, &RULE), Step::Done);
        assert_eq!(decide(ClientError, 1, true, None, &RULE), Step::Done);
        assert_eq!(
            decide(RateLimited, 1, false, Some(500), &RULE),
            Step::Retry(500)
        );
        assert_eq!(
            decide(RateLimited, 1, false, None, &RULE),
            Step::Retry(1_000)
        );
        assert_eq!(
            decide(RateLimited, 1, false, Some(1_001), &RULE),
            Step::GiveUp
        );
        assert_eq!(decide(RateLimited, 3, false, Some(1), &RULE), Step::GiveUp);
        assert_eq!(decide(ServerError, 1, false, None, &RULE), Step::GiveUp);
        assert_eq!(decide(ServerError, 2, true, None, &RULE), Step::Retry(200));
        assert_eq!(decide(NetworkError, 1, true, None, &RULE), Step::Retry(100));
        assert_eq!(decide(NetworkError, 3, true, None, &RULE), Step::GiveUp);
    }

    #[test]
    fn retry_after_saturates() {
        assert_eq!(retry_after_ms(3), 3_000);
        assert_eq!(retry_after_ms(u64::MAX), u64::MAX);
    }

    fn outcome() -> impl Strategy<Value = Outcome> {
        prop_oneof![
            Just(Outcome::Success),
            Just(Outcome::RateLimited),
            Just(Outcome::ServerError),
            Just(Outcome::NetworkError),
            Just(Outcome::ClientError),
        ]
    }

    proptest! {
        // Same properties as the Verus `ensures` clauses.
        #[test]
        fn decide_invariants(
            o in outcome(),
            attempt in 0u32..20,
            idem: bool,
            ra in proptest::option::of(0u64..5_000),
            max_attempts in 1u32..=MAX_ATTEMPTS,
            init in 0u64..10_000,
            cap in 0u64..10_000,
        ) {
            let rule = RetryRule { max_attempts, initial_backoff_ms: init, max_wait_ms: cap };
            match decide(o, attempt, idem, ra, &rule) {
                Step::Retry(w) => {
                    prop_assert!(attempt < max_attempts);
                    prop_assert!(w <= cap);
                    prop_assert!(o == Outcome::RateLimited || idem);
                }
                Step::Done => prop_assert!(matches!(o, Outcome::Success | Outcome::ClientError)),
                Step::GiveUp => prop_assert!(!matches!(o, Outcome::Success | Outcome::ClientError)),
            }
        }

        #[test]
        fn fresh_is_symmetric(a: u64, b: u64, t: u64) {
            prop_assert_eq!(timestamp_fresh(a, b, t), timestamp_fresh(b, a, t));
            prop_assert_eq!(timestamp_fresh(a, b, t), a.abs_diff(b) <= t);
        }

        #[test]
        fn backoff_is_monotonic_and_capped(init in 0u64..1_000_000, n in 1u32..70, cap: u64) {
            let a = backoff_ms(init, n, cap);
            prop_assert!(a <= cap);
            prop_assert!(a <= backoff_ms(init, n + 1, cap));
        }
    }
}
