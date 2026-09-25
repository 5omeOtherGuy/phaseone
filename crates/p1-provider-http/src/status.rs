//! HTTP status classification and server retry hints.
//!
//! Portable (ADR-0071): a provider WebAssembly component classifies responses
//! with these the same way the native driver does, without pulling in the
//! native retry loop.

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
    header_seconds(headers, "retry-after")
}

/// Read an integer-seconds rate-limit reset hint, preferring `Retry-After`.
pub fn reset_after(headers: &[(String, String)]) -> Option<Duration> {
    retry_after(headers).or_else(|| {
        [
            "x-ratelimit-reset",
            "x-ratelimit-reset-requests",
            "x-ratelimit-reset-tokens",
        ]
        .into_iter()
        .find_map(|name| header_seconds(headers, name))
    })
}

fn header_seconds(headers: &[(String, String)], expected: &str) -> Option<Duration> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(expected))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn reset_hint_prefers_retry_after_then_accepts_rate_limit_reset() {
        assert_eq!(
            reset_after(&[
                ("x-ratelimit-reset".into(), "20".into()),
                ("retry-after".into(), "10".into()),
            ]),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            reset_after(&[("X-RateLimit-Reset-Requests".into(), "42".into())]),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            reset_after(&[("x-ratelimit-reset".into(), "later".into())]),
            None
        );
    }
}
