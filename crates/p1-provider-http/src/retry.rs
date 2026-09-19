//! Status classification and the transient-retry policy.
//!
//! Both adapters share one retry budget and one backoff shape. Nothing here
//! sleeps or touches the network: [`RetryPolicy::delay`] is a pure function of
//! the policy, the 1-based retry number and an optional server hint, so the
//! retry-loop tests can run on a paused clock.

use std::time::Duration;

/// What to do about an HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpClass {
    /// 2xx: read the body.
    Success,
    /// 401/403: one credential refresh and one re-send.
    Reauth,
    /// 408/425/429/5xx: transient, retry with backoff inside the shared budget.
    Retry,
    /// Anything else non-2xx: surface immediately.
    Fatal,
}

/// Classify an HTTP status. The enumeration is authoritative
/// (`docs/design/providers.md`): `401|403` → `Reauth`; `408|425|429|500..=599` →
/// `Retry`; any other non-2xx (including 3xx) → `Fatal`.
pub fn classify_status(status: u16) -> HttpClass {
    match status {
        200..=299 => HttpClass::Success,
        401 | 403 => HttpClass::Reauth,
        408 | 425 | 429 => HttpClass::Retry,
        500..=599 => HttpClass::Retry,
        _ => HttpClass::Fatal,
    }
}

/// Read an integer-seconds `Retry-After` header. The HTTP-date form is uncommon
/// for these routes and is ignored (as in the donor).
pub fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// The shared transient-retry budget and backoff shape.
///
/// Defaults: 3 retries, 2 s base doubling, 60 s cap, up to 250 ms of jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum transient retries before the loop yields the last failure.
    pub max_retries: u32,
    /// Base backoff, doubled per retry.
    pub base: Duration,
    /// Backoff ceiling. A `Retry-After` hint is bounded at 4× this.
    pub cap: Duration,
    /// Maximum jitter added to an exponentially computed delay.
    pub jitter: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(60),
            jitter: Duration::from_millis(250),
        }
    }
}

impl RetryPolicy {
    /// Delay before retry number `retry` (1-based).
    ///
    /// A server `Retry-After` is honoured exactly, clamped to 4× [`Self::cap`];
    /// jitter is deliberately NOT added to a hint, so the delay is predictable
    /// and the hint is never pushed past its clamp. Otherwise the delay is
    /// `base * 2^(retry-1)`, clamped to [`Self::cap`], plus jitter.
    pub fn delay(&self, retry: u32, retry_after: Option<Duration>) -> Duration {
        let retry = retry.max(1);
        if let Some(hint) = retry_after {
            return hint.min(self.cap.saturating_mul(4));
        }
        let shift = retry.saturating_sub(1).min(10);
        let exponential = self
            .base
            .checked_mul(1u32 << shift)
            .unwrap_or(self.cap)
            .min(self.cap);
        exponential + self.jitter_for(retry)
    }

    /// Jitter for retry `retry`, drawn from a tiny xorshift seeded by the policy
    /// fields (so it is deterministic for a given policy and reproducible in
    /// tests) rather than from a global RNG.
    fn jitter_for(&self, retry: u32) -> Duration {
        let bound = self.jitter.as_millis() as u64;
        if bound == 0 {
            return Duration::ZERO;
        }
        let mut state = 0x9E37_79B9_7F4A_7C15
            ^ (u64::from(self.max_retries)).wrapping_mul(0xBF58_476D_1CE4_E5B9)
            ^ (self.base.as_millis() as u64).wrapping_mul(0x94D0_49BB_1331_11EB)
            ^ (self.cap.as_millis() as u64).rotate_left(17)
            ^ bound.rotate_left(31)
            ^ (u64::from(retry)).wrapping_mul(0xD6E8_FEB8_6659_FD93);
        if state == 0 {
            state = 0x2545_F491_4F6C_DD1D;
        }
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        Duration::from_millis(state % bound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_matches_the_spec() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.base, Duration::from_secs(2));
        assert_eq!(policy.cap, Duration::from_secs(60));
        assert_eq!(policy.jitter, Duration::from_millis(250));
    }

    #[test]
    fn classifies_every_status_family() {
        assert_eq!(classify_status(200), HttpClass::Success);
        assert_eq!(classify_status(204), HttpClass::Success);
        assert_eq!(classify_status(301), HttpClass::Fatal);
        assert_eq!(classify_status(400), HttpClass::Fatal);
        assert_eq!(classify_status(401), HttpClass::Reauth);
        assert_eq!(classify_status(403), HttpClass::Reauth);
        assert_eq!(classify_status(404), HttpClass::Fatal);
        assert_eq!(classify_status(408), HttpClass::Retry);
        assert_eq!(classify_status(425), HttpClass::Retry);
        assert_eq!(classify_status(429), HttpClass::Retry);
        assert_eq!(classify_status(500), HttpClass::Retry);
        assert_eq!(classify_status(503), HttpClass::Retry);
        assert_eq!(classify_status(599), HttpClass::Retry);
        assert_eq!(classify_status(600), HttpClass::Fatal);
    }

    #[test]
    fn parses_integer_retry_after_case_insensitively() {
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("Retry-After".to_string(), " 7 ".to_string()),
        ];
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(&[]), None);
        let http_date = vec![(
            "retry-after".to_string(),
            "Wed, 21 Oct 2015 07:28:00 GMT".to_string(),
        )];
        assert_eq!(retry_after(&http_date), None);
    }

    #[test]
    fn backoff_doubles_from_the_base_and_clamps_to_the_cap() {
        let policy = RetryPolicy {
            jitter: Duration::ZERO,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.delay(1, None), Duration::from_secs(2));
        assert_eq!(policy.delay(2, None), Duration::from_secs(4));
        assert_eq!(policy.delay(3, None), Duration::from_secs(8));
        assert_eq!(policy.delay(20, None), Duration::from_secs(60));
    }

    #[test]
    fn retry_after_is_honoured_exactly_and_clamped() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.delay(1, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(
            policy.delay(1, Some(Duration::from_secs(86_400))),
            Duration::from_secs(240)
        );
    }

    #[test]
    fn jitter_is_bounded_and_deterministic() {
        let policy = RetryPolicy::default();
        for retry in 1..=10 {
            let first = policy.jitter_for(retry);
            assert_eq!(first, policy.jitter_for(retry));
            assert!(first < policy.jitter);
        }
    }
}
