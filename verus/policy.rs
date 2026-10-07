//! Verus spec and proof for `src/policy.rs`.
//!
//! Keep this file in step with `src/policy.rs`.
//! Run: `verus verus/policy.rs`.

use vstd::prelude::*;

verus! {

/// Slack limit: reject a request older or newer than 5 minutes.
pub const DEFAULT_TOLERANCE_SECS: u64 = 300;
/// Largest number of attempts for one Web API call.
pub const MAX_ATTEMPTS: u32 = 10;
/// Wait for a 429 with no `Retry-After` header.
pub const DEFAULT_RETRY_AFTER_MS: u64 = 1_000;

// ---------------------------------------------------------------- timestamp

/// Spec: the distance between two instants.
pub open spec fn spec_abs_diff(a: int, b: int) -> int {
    if a >= b { a - b } else { b - a }
}

/// Spec: a request is fresh when its timestamp is within the tolerance.
pub open spec fn spec_fresh(now: int, ts: int, tolerance: int) -> bool {
    spec_abs_diff(now, ts) <= tolerance
}

/// Returns `true` when `ts` is within `tolerance` seconds of `now`.
pub fn timestamp_fresh(now: u64, ts: u64, tolerance: u64) -> (r: bool)
    ensures
        r == spec_fresh(now as int, ts as int, tolerance as int),
{
    let diff = if now >= ts { now - ts } else { ts - now };
    diff <= tolerance
}

/// Freshness does not depend on the order of the two instants.
proof fn lemma_fresh_symmetric(a: int, b: int, t: int)
    ensures
        spec_fresh(a, b, t) == spec_fresh(b, a, t),
{
}

/// A larger tolerance accepts all that a smaller tolerance accepts.
proof fn lemma_fresh_monotonic(now: int, ts: int, t1: int, t2: int)
    requires
        t1 <= t2,
        spec_fresh(now, ts, t1),
    ensures
        spec_fresh(now, ts, t2),
{
}

/// A zero tolerance accepts only the same second.
proof fn lemma_fresh_zero(now: int, ts: int)
    ensures
        spec_fresh(now, ts, 0) == (now == ts),
{
}

// ------------------------------------------------------------------ backoff

pub open spec fn spec_min(a: int, b: int) -> int {
    if a <= b { a } else { b }
}

/// Spec: `initial * 2^(attempt - 1)`, capped at `cap`.
pub open spec fn spec_backoff(initial: int, attempt: int, cap: int) -> int
    decreases attempt,
{
    if attempt <= 1 {
        spec_min(initial, cap)
    } else {
        spec_min(spec_backoff(initial, attempt - 1, cap) * 2, cap)
    }
}

proof fn lemma_backoff_bounds(initial: int, attempt: int, cap: int)
    requires
        initial >= 0,
        cap >= 0,
    ensures
        0 <= spec_backoff(initial, attempt, cap) <= cap,
    decreases attempt,
{
    if attempt > 1 {
        lemma_backoff_bounds(initial, attempt - 1, cap);
    }
}

/// The backoff does not decrease from one attempt to the next.
proof fn lemma_backoff_monotonic(initial: int, attempt: int, cap: int)
    requires
        initial >= 0,
        cap >= 0,
        attempt >= 1,
    ensures
        spec_backoff(initial, attempt, cap) <= spec_backoff(initial, attempt + 1, cap),
{
    lemma_backoff_bounds(initial, attempt, cap);
}

/// Once the backoff reaches the cap, it stays at the cap.
proof fn lemma_backoff_stays_at_cap(initial: int, i: int, j: int, cap: int)
    requires
        cap >= 0,
        1 <= i <= j,
        spec_backoff(initial, i, cap) == cap,
    ensures
        spec_backoff(initial, j, cap) == cap,
    decreases j - i,
{
    if j > i {
        lemma_backoff_stays_at_cap(initial, i, j - 1, cap);
    }
}

/// Once the backoff is zero, it stays zero.
proof fn lemma_backoff_stays_at_zero(initial: int, i: int, j: int, cap: int)
    requires
        cap >= 0,
        1 <= i <= j,
        spec_backoff(initial, i, cap) == 0,
    ensures
        spec_backoff(initial, j, cap) == 0,
    decreases j - i,
{
    if j > i {
        lemma_backoff_stays_at_zero(initial, i, j - 1, cap);
    }
}

/// Returns the wait in ms before retry number `attempt` (1-based).
pub fn backoff_ms(initial: u64, attempt: u32, cap: u64) -> (r: u64)
    ensures
        r <= cap,
        r == spec_backoff(initial as int, attempt as int, cap as int),
{
    let mut b: u64 = if initial <= cap { initial } else { cap };
    let mut i: u32 = 1;
    while i < attempt
        invariant
            1 <= i,
            attempt == 0 ==> i == 1,
            attempt >= 1 ==> i <= attempt,
            b <= cap,
            b == spec_backoff(initial as int, i as int, cap as int),
        decreases attempt - i,
    {
        // At the cap or at zero: stop early. A large `attempt` costs no time.
        if b == cap {
            proof {
                lemma_backoff_stays_at_cap(initial as int, i as int, attempt as int, cap as int);
            }
            return b;
        }
        if b == 0 {
            proof {
                lemma_backoff_stays_at_zero(initial as int, i as int, attempt as int, cap as int);
            }
            return b;
        }
        // Compare without overflow: `cap - b <= b` is `2b >= cap`.
        b = if cap - b <= b { cap } else { b * 2 };
        i = i + 1;
    }
    // Attempt 0 and attempt 1 have the same spec value.
    assert(attempt == 0 ==> spec_backoff(initial as int, attempt as int, cap as int)
        == spec_backoff(initial as int, 1, cap as int));
    b
}

// ------------------------------------------------------------ retry decision

/// The class of one Web API attempt.
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
pub enum Step {
    /// Return the result as it is.
    Done,
    /// Wait this many ms, then try again.
    Retry(u64),
    /// Stop. Return the last error.
    GiveUp,
}

/// Retry rules for one call.
pub struct RetryRule {
    /// Attempts in total, 1 to `MAX_ATTEMPTS`.
    pub max_attempts: u32,
    /// First backoff for 5xx and network errors.
    pub initial_backoff_ms: u64,
    /// Largest wait before a retry.
    pub max_wait_ms: u64,
}

/// Spec: a retry is allowed for this outcome.
pub open spec fn spec_retryable(outcome: Outcome, idempotent: bool) -> bool {
    match outcome {
        Outcome::RateLimited => true,
        Outcome::ServerError | Outcome::NetworkError => idempotent,
        _ => false,
    }
}

/// Decides the next step after attempt number `attempt` (1-based).
pub fn decide(
    outcome: Outcome,
    attempt: u32,
    idempotent: bool,
    retry_after_ms: Option<u64>,
    rule: &RetryRule,
) -> (s: Step)
    ensures
        // Bounded attempts.
        s is Retry ==> attempt < rule.max_attempts,
        // Bounded wait.
        s is Retry ==> s->Retry_0 <= rule.max_wait_ms,
        // A write call never repeats after a 5xx or a network error.
        s is Retry ==> spec_retryable(outcome, idempotent),
        // Success and client errors end the call.
        (outcome is Success || outcome is ClientError) ==> s is Done,
        // A 429 with a wait in the cap and attempts left always retries.
        (outcome is RateLimited && attempt < rule.max_attempts && (match retry_after_ms {
            Some(w) => w <= rule.max_wait_ms,
            None => DEFAULT_RETRY_AFTER_MS <= rule.max_wait_ms,
        })) ==> s is Retry,
        // A 429 retry waits exactly as long as Slack asks.
        (outcome is RateLimited && s is Retry) ==> s->Retry_0 == (match retry_after_ms {
            Some(w) => w,
            None => DEFAULT_RETRY_AFTER_MS,
        }),
{
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
        },
        Outcome::ServerError | Outcome::NetworkError => {
            if idempotent && attempt < rule.max_attempts {
                let b = backoff_ms(rule.initial_backoff_ms, attempt, rule.max_wait_ms);
                Step::Retry(b)
            } else {
                Step::GiveUp
            }
        },
    }
}

/// Converts `Retry-After` seconds to ms. It saturates and does not overflow.
pub fn retry_after_ms(secs: u64) -> (r: u64)
    ensures
        secs <= u64::MAX / 1000 ==> r == secs * 1000,
        secs > u64::MAX / 1000 ==> r == u64::MAX,
{
    if secs <= u64::MAX / 1000 {
        secs * 1000
    } else {
        u64::MAX
    }
}

/// A call stops after at most `max_attempts` attempts: the attempt counter
/// starts at 1 and each `Retry` adds 1, and `Retry` needs `attempt < max`.
proof fn lemma_attempts_bounded(attempt: u32, max_attempts: u32)
    requires
        attempt < max_attempts,
    ensures
        attempt + 1 <= max_attempts,
{
}

fn main() {
}

} // verus!
