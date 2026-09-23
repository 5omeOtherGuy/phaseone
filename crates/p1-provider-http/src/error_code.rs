use p1_contracts::ProviderErrorKind;
use p1_contracts::serde_json::Value;

/// Return a short, token-shaped, non-sensitive provider code suitable for display.
///
/// Trimming is retained for compatibility. Only ASCII alphanumerics, `_`, `-`,
/// and `.` are accepted; values containing credential-like fragments are rejected.
pub fn safe_code(value: &str) -> Option<&str> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let sensitive = ["secret", "password", "sk-"];
    (!value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        && !sensitive.iter().any(|fragment| lower.contains(fragment)))
    .then_some(value)
}

/// Extract a safe code from JSON error bodies.
///
/// Accepts string `/error/code`, `/error/type`, or top-level `code`/`type`;
/// candidates are tried in that order, and only [`safe_code`] values are returned.
pub fn http_error_code(body: &[u8]) -> Option<String> {
    let value: Value = p1_contracts::serde_json::from_slice(body).ok()?;
    ["/error/code", "/error/type", "/code", "/type"]
        .iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .find_map(safe_code)
        .map(str::to_string)
}

/// Shared default HTTP status classification for provider adapters.
pub fn kind_for_status(status: u16) -> Option<ProviderErrorKind> {
    Some(match status {
        401 | 403 => ProviderErrorKind::Authentication,
        429 => ProviderErrorKind::RateLimited,
        408 | 425 | 500..=599 => ProviderErrorKind::Transport,
        400..=499 => ProviderErrorKind::InvalidRequest,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_codes_are_token_shaped_and_bounded() {
        assert_eq!(safe_code("  valid_code-1.ok  "), Some("valid_code-1.ok"));
        for invalid in [
            "",
            "has spaces",
            "punctuation!",
            "código",
            &"a".repeat(65),
            "mysecret",
            "password",
            "sk-abcdef",
        ] {
            assert_eq!(safe_code(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn extracts_first_safe_code_from_supported_shapes() {
        assert_eq!(
            http_error_code(br#"{"error":{"code":"has spaces","type":"safe_type"}}"#),
            Some("safe_type".into())
        );
        assert_eq!(
            http_error_code(br#"{"code":"top_code","type":"top_type"}"#),
            Some("top_code".into())
        );
        assert_eq!(http_error_code(b"not json"), None);
    }

    #[test]
    fn classifies_http_statuses() {
        assert_eq!(
            kind_for_status(401),
            Some(ProviderErrorKind::Authentication)
        );
        assert_eq!(kind_for_status(429), Some(ProviderErrorKind::RateLimited));
        assert_eq!(kind_for_status(503), Some(ProviderErrorKind::Transport));
        assert_eq!(
            kind_for_status(400),
            Some(ProviderErrorKind::InvalidRequest)
        );
        assert_eq!(kind_for_status(200), None);
    }
}
