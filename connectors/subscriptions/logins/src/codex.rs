//! The Codex CLI login (`codex-oauth`): the CLI's `auth.json` and its token refresh.
//!
//! p1 builds no login flow: it reuses the auth file exactly as the donor reused the CLI login.
//! The access token's expiry and the ChatGPT account are read from its JWT claims; a signature
//! is never verified. A refresh is written back with the rotated tokens and `last_refresh`,
//! every other field kept.

use serde_json::{Value, json};

use crate::{Login, non_empty, refused};

const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The Codex CLI's public OAuth client id (from its source; never a secret).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth";

/// The Codex CLI login.
pub struct Codex;

impl Login for Codex {
    fn describe(&self) -> String {
        json!({
            "method": {
                "owner": "the Codex CLI (`codex login`)",
                "store": "oauth",
                "usage_probe": "codex",
            },
            "login": {
                "title": "Codex login",
                "guidance": "run `codex login`",
                "dir_env": "CODEX_HOME",
                "home_dir": ".codex",
                "file": "auth.json",
                // The lock file older p1 processes take: `auth.json` → `auth.lock`.
                "lock": "auth.lock",
            },
        })
        .to_string()
    }

    fn read_login(&self, file: &[u8], _key: Option<&str>) -> Result<String, String> {
        let document: Value = serde_json::from_slice(file)
            .map_err(|_| "the Codex auth file is not valid JSON".to_string())?;
        let tokens = document
            .get("tokens")
            .and_then(Value::as_object)
            .ok_or_else(no_tokens)?;
        let access = non_empty(tokens.get("access_token")).ok_or_else(no_tokens)?;
        let expires_at_ms = jwt_exp(&access)
            .map_err(|()| "the Codex access token has an invalid expiry claim".to_string())?
            .map(|seconds| seconds.saturating_mul(1000));
        let account_id = non_empty(tokens.get("account_id")).or_else(|| jwt_account_id(&access));
        Ok(json!({
            "access": access,
            "refresh": non_empty(tokens.get("refresh_token")),
            "expires_at_ms": expires_at_ms,
            "account_id": account_id,
        })
        .to_string())
    }

    fn refresh_request(&self, refresh_token: &str, _file: Option<&[u8]>) -> Result<String, String> {
        Ok(json!({
            "url": TOKEN_URL,
            "headers": [
                ["Content-Type", "application/x-www-form-urlencoded"],
                ["Accept", "application/json"],
            ],
            "body": format!(
                "grant_type=refresh_token&refresh_token={}&client_id={}",
                percent_encode(refresh_token),
                percent_encode(CLIENT_ID),
            ),
        })
        .to_string())
    }

    fn refresh_response(&self, body: &[u8], sent_at_ms: u64) -> String {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return refused("token refresh response was not valid JSON");
        };
        let Some(access) = non_empty(value.get("access_token")) else {
            return json!({
                "access": {"err": "token refresh response had no access token"},
                "refresh": non_empty(value.get("refresh_token")),
                "expires_at_ms": {"ok": null},
            })
            .to_string();
        };
        // The JWT's own expiry; an opaque token falls back to the response's lifetime.
        let expires_at_ms = match jwt_exp(&access) {
            Ok(Some(seconds)) => json!({"ok": seconds.saturating_mul(1000)}),
            Ok(None) => json!({"ok": value
                .get("expires_in")
                .and_then(Value::as_u64)
                .map(|seconds| sent_at_ms.saturating_add(seconds.saturating_mul(1000)))}),
            Err(()) => json!({"err": "the token refresh returned a token with an invalid expiry"}),
        };
        json!({
            "access": {"ok": access},
            "refresh": non_empty(value.get("refresh_token")),
            "expires_at_ms": expires_at_ms,
            "account_id": jwt_account_id(&access),
        })
        .to_string()
    }

    fn merge_login(
        &self,
        file: &[u8],
        body: &[u8],
        _sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        let mut document: Value = serde_json::from_slice(file)
            .map_err(|_| "the Codex auth file is not valid JSON".to_string())?;
        let response: Value = serde_json::from_slice(body)
            .map_err(|_| "token refresh response was not valid JSON".to_string())?;
        // The rotated refresh token always; the new access (and id) token when the host kept
        // it. Without one the old access token stays: it is expired or rejected, so the next
        // access refreshes again with the rotated refresh token instead of losing the login.
        if let Some(tokens) = document.get_mut("tokens").and_then(Value::as_object_mut) {
            if keep_access && let Some(access) = non_empty(response.get("access_token")) {
                tokens.insert("access_token".to_string(), Value::String(access));
                if let Some(id_token) = non_empty(response.get("id_token")) {
                    tokens.insert("id_token".to_string(), Value::String(id_token));
                }
            }
            if let Some(refresh) = non_empty(response.get("refresh_token")) {
                tokens.insert("refresh_token".to_string(), Value::String(refresh));
            }
        }
        if let Some(object) = document.as_object_mut() {
            object.insert(
                "last_refresh".to_string(),
                Value::String(format_rfc3339_utc(now_ms / 1000)),
            );
        }
        serde_json::to_vec_pretty(&document)
            .map_err(|_| "failed to write the Codex auth file".to_string())
    }
}

fn no_tokens() -> String {
    "no ChatGPT tokens in the Codex auth file; run `codex login` with ChatGPT (an API key alone \
     is not valid for this route)"
        .to_string()
}

/// Percent-encode a form value by hand: unreserved characters pass through, a space becomes
/// `+`, everything else is `%XX` uppercase.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// The `exp` claim in seconds. An opaque token — no decodable JWT payload, or a payload
/// without `exp` — has none; the 401 path is the backstop. A payload whose `exp` is not a
/// timestamp is an error, never a silently fresh token.
fn jwt_exp(token: &str) -> Result<Option<u64>, ()> {
    let Some(payload) = jwt_payload(token) else {
        return Ok(None);
    };
    match payload.get("exp") {
        None | Some(Value::Null) => Ok(None),
        // RFC 7519 NumericDate may carry a fraction.
        Some(exp) => exp
            .as_u64()
            .or_else(|| {
                exp.as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .map(|seconds| seconds as u64)
            })
            .map(Some)
            .ok_or(()),
    }
}

fn jwt_account_id(token: &str) -> Option<String> {
    jwt_payload(token)?
        .get(ACCOUNT_ID_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// Minimal base64url (no padding) decoder.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = sextet(byte)?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// Seconds since the Unix epoch as RFC 3339 UTC, without a date crate.
fn format_rfc3339_utc(seconds: u64) -> String {
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64url_payloads() {
        assert_eq!(base64url_decode("aGk").unwrap(), b"hi".to_vec());
        assert_eq!(base64url_decode("aGk=").unwrap(), b"hi".to_vec());
        assert!(base64url_decode("!!!").is_none());
    }

    #[test]
    fn reads_the_account_id_from_the_jwt_claim() {
        // {"https://api.openai.com/auth":{"chatgpt_account_id":"acc_jwt"}}
        let token = "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjX2p3dCJ9fQ.sig";
        assert_eq!(jwt_account_id(token).as_deref(), Some("acc_jwt"));
        assert_eq!(jwt_account_id("not-a-jwt"), None);
    }

    #[test]
    fn formats_rfc3339_utc_by_hand() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn percent_encodes_form_values() {
        assert_eq!(percent_encode("abc-._~123"), "abc-._~123");
        assert_eq!(percent_encode("a b"), "a+b");
        assert_eq!(percent_encode("a/b?c&d"), "a%2Fb%3Fc%26d");
    }

    #[test]
    fn a_merge_writes_the_rotation_and_keeps_every_other_field() {
        let file = br#"{"tokens":{"access_token":"old","refresh_token":"r1","id_token":"i1","account_id":"a"},"x":1}"#;
        let body = br#"{"access_token":"new","refresh_token":"r2","id_token":"i2"}"#;
        let merged: Value = serde_json::from_slice(
            &Codex
                .merge_login(file, body, 0, 1_700_000_000_000, true)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(merged["tokens"]["access_token"], "new");
        assert_eq!(merged["tokens"]["id_token"], "i2");
        assert_eq!(merged["tokens"]["refresh_token"], "r2");
        assert_eq!(merged["tokens"]["account_id"], "a");
        assert_eq!(merged["x"], 1);
        assert_eq!(merged["last_refresh"], "2023-11-14T22:13:20Z");
        let refused: Value =
            serde_json::from_slice(&Codex.merge_login(file, body, 0, 0, false).unwrap()).unwrap();
        assert_eq!(refused["tokens"]["access_token"], "old");
        assert_eq!(refused["tokens"]["id_token"], "i1");
        assert_eq!(refused["tokens"]["refresh_token"], "r2");
    }
}
