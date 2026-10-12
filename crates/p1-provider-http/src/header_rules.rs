//! The header-name rules every party shares (portable): which header names a provider
//! component's lowered request may never carry (the broker refuses them), and which a route's
//! own data — a declared dialect's or client identity's headers (ADR-0214) — may set. One rule,
//! so a route file is refused when it loads for a header every request would be refused for,
//! and a secret-shaped name never travels as route data the echo scrub does not cover.

/// Headers a component may never set. Most carry a credential, and the broker is
/// the only party that attaches one; the rest fix the request's authority or
/// framing. `forwarded` and `x-original-host` are the proxy headers a front end
/// may trust to choose the authority a credentialed request reaches, and the whole
/// `x-forwarded-` family is refused by prefix in `allowed_name`, so a component may
/// never place one either.
pub(crate) const FORBIDDEN_HEADERS: [&str; 18] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-api-key",
    "api-key",
    "host",
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
    "te",
    "trailer",
    "upgrade",
    "expect",
    "via",
    "forwarded",
    "x-original-host",
];

/// The proxy headers a front end may honor to choose the target a credentialed
/// request reaches. A reverse proxy can read any `X-Forwarded-*` field (Prefix,
/// Uri, ...), so the family is reserved by prefix rather than by an enumeration
/// that a new member would slip past.
pub(crate) const FORWARDED_PREFIX: &str = "x-forwarded-";

/// Case-insensitive prefix test for the header families the broker reserves by
/// family: the proxy `x-forwarded-` fields and the connector's own
/// `sec-websocket-` fields.
pub(crate) fn has_ignored_prefix(name: &str, prefix: &str) -> bool {
    name.len() >= prefix.len()
        && name.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The WebSocket connector's own handshake fields. `into_client_request` generates
/// the key, the version and any extension field, so a component that names one
/// would replace or contradict them and produce a wrong-protocol handshake. Matched
/// by prefix so a future `Sec-WebSocket-*` request field is reserved too.
pub(crate) const WS_HANDSHAKE_PREFIX: &str = "sec-websocket-";

/// Whether `name` is shaped like a header that carries a secret: one of the credential
/// headers, or an `x-auth*` field. The host refuses such a name as a route's static header,
/// and route data may not name one either.
pub fn is_secret_header_name(name: &str) -> bool {
    has_ignored_prefix(name, "x-auth")
        || [
            "authorization",
            "proxy-authorization",
            "x-api-key",
            "api-key",
            "cookie",
        ]
        .iter()
        .any(|secret| name.eq_ignore_ascii_case(secret))
}

/// Whether route data may name header `name`: an HTTP token (RFC 9110), no header the broker
/// refuses in a lowered request, no `x-forwarded-` or `sec-websocket-` field, and not shaped
/// like a secret.
pub fn declarable_header(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !FORBIDDEN_HEADERS
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        && !has_ignored_prefix(name, FORWARDED_PREFIX)
        && !has_ignored_prefix(name, WS_HANDSHAKE_PREFIX)
        && !is_secret_header_name(name)
}

/// Whether `value` is a header value route data may carry: non-empty visible ASCII and spaces.
pub fn declarable_header_value(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (32..=126).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_data_names_no_header_the_broker_or_the_host_refuses() {
        for refused in [
            "Cookie",
            "proxy-authorization",
            "forwarded",
            "X-Forwarded-For",
            "connection",
            "upgrade",
            "Sec-WebSocket-Key",
            "x-auth-token",
            "api-key",
            "a b",
            "",
        ] {
            assert!(!declarable_header(refused), "{refused}");
        }
        for allowed in ["x-app", "originator", "OpenAI-Beta", "x-opencode-session"] {
            assert!(declarable_header(allowed), "{allowed}");
        }
        assert!(!declarable_header_value("line\nbreak"));
        assert!(declarable_header_value("opencode/1.18.31 runtime/bun"));
    }
}
