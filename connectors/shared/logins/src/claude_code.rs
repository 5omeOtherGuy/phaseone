//! The Claude Code login (`claude-code-oauth`): Claude Code's `.credentials.json` and its token
//! refresh.
//!
//! The file layout, expiry unit, refresh request, token URL, client id and scopes are taken
//! from the donor's `src/mimir/auth/anthropic.rs`, never from any real file. Both the nested
//! (`{"claudeAiOauth":{…}}`) and the older flat shape are read; a refresh is written back in
//! the shape the file has, every other field kept.

use serde_json::{Value, json};

use crate::{Login, REFRESH_MARGIN_MS, non_empty, refused};

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Scopes requested when a stored credential records none (the runtime scopes Claude Code
/// itself uses on refresh; the login-only `org:create_api_key` scope is deliberately not part
/// of a refresh).
const DEFAULT_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// A year: no OAuth access token p1 refreshes lives longer.
const MAX_LIFETIME_SECS: u64 = 366 * 24 * 3600;

/// The Claude Code login.
pub struct ClaudeCode;

/// What the login file holds.
struct Stored {
    access: String,
    refresh: Option<String>,
    expires_ms: Option<u64>,
    /// The raw `scopes` value, preserved so the refresh requests the same scopes.
    scopes: Option<Value>,
}

/// A refresh response, each field on its own.
struct Response {
    access: Result<String, &'static str>,
    refresh: Option<String>,
    lifetime_ms: Result<u64, &'static str>,
    scope: Option<String>,
}

impl Login for ClaudeCode {
    fn describe(&self) -> String {
        json!({
            "method": {
                "owner": "the Claude Code CLI (`claude`)",
                "store": "oauth",
                "usage_probe": "claude",
            },
            "login": {
                "title": "Claude Code login",
                "guidance": "run Claude Code login",
                "dir_env": "CLAUDE_CONFIG_DIR",
                "home_dir": ".claude",
                "file": ".credentials.json",
                "lock": ".credentials.json.lock",
                "route_dir": true,
            },
        })
        .to_string()
    }

    fn read_login(&self, file: &[u8], _key: Option<&str>) -> Result<String, String> {
        let stored = parse(file)?;
        Ok(json!({
            "access": stored.access,
            "refresh": stored.refresh,
            "expires_at_ms": stored.expires_ms,
        })
        .to_string())
    }

    fn refresh_request(&self, refresh_token: &str, file: Option<&[u8]>) -> Result<String, String> {
        // A borrowed login asks for the scopes it records; p1's store records none.
        let scope = match file {
            Some(file) => scopes_for(parse(file)?.scopes.as_ref()),
            None => DEFAULT_SCOPES.to_string(),
        };
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": scope,
        });
        Ok(json!({
            "url": TOKEN_URL,
            "headers": [["content-type", "application/json"], ["anthropic-beta", OAUTH_BETA]],
            "body": body.to_string(),
        })
        .to_string())
    }

    fn refresh_response(&self, body: &[u8], sent_at_ms: u64) -> String {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return refused("the Claude Code token refresh response is malformed");
        };
        let response = parse_response(&value);
        let checked = |result: Result<Value, &str>| match result {
            Ok(value) => json!({"ok": value}),
            Err(reason) => json!({"err": reason}),
        };
        json!({
            "access": checked(response.access.map(Value::from)),
            "refresh": response.refresh,
            "expires_at_ms": checked(
                response
                    .lifetime_ms
                    .map(|lifetime| Value::from(sent_at_ms.saturating_add(lifetime)))
            ),
        })
        .to_string()
    }

    fn merge_login(
        &self,
        file: &[u8],
        body: &[u8],
        sent_at_ms: u64,
        _now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        let stored = parse(file).ok();
        let response = serde_json::from_slice::<Value>(body)
            .map(|value| parse_response(&value))
            .map_err(|_| "the Claude Code token refresh response is malformed".to_string())?;
        let usable = match (&response.access, &response.lifetime_ms) {
            (Ok(access), Ok(lifetime)) if keep_access => {
                Some((access.as_str(), sent_at_ms.saturating_add(*lifetime)))
            }
            _ => None,
        };
        merge_document(file, stored.as_ref(), &response, usable)
    }
}

/// Both the nested and the older flat shape. Reasons name no value.
fn parse(file: &[u8]) -> Result<Stored, String> {
    let document: Value = serde_json::from_slice(file)
        .map_err(|_| "Claude Code credentials are malformed".to_string())?;
    let oauth = document.get("claudeAiOauth").unwrap_or(&document);
    let access = non_empty(oauth.get("accessToken"))
        .ok_or_else(|| "Claude Code credentials are missing accessToken".to_string())?;
    // Absent or null is "no expiry" (an older flat shape); anything but a millisecond
    // timestamp is an error, never a silently fresh token.
    let expires_ms = match oauth.get("expiresAt") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            "Claude Code credentials have an invalid expiresAt (not a millisecond timestamp)"
                .to_string()
        })?),
    };
    Ok(Stored {
        access,
        refresh: non_empty(oauth.get("refreshToken")),
        expires_ms,
        scopes: oauth.get("scopes").cloned(),
    })
}

/// Space-joined scopes, falling back to the Claude Code defaults when the credential records
/// none (or only blanks).
fn scopes_for(scopes: Option<&Value>) -> String {
    match scopes {
        Some(Value::Array(items)) => {
            let scopes: Vec<&str> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .collect();
            if scopes.is_empty() {
                DEFAULT_SCOPES.to_string()
            } else {
                scopes.join(" ")
            }
        }
        Some(Value::String(scopes)) if !scopes.trim().is_empty() => scopes.trim().to_string(),
        _ => DEFAULT_SCOPES.to_string(),
    }
}

/// The lifetime a refresh response grants, as milliseconds to add to the time the request was
/// SENT: a lifetime that would already count as due for refresh is refused (every later access
/// would rotate it again), and an implausibly long one is capped.
fn lifetime_ms(expires_in_secs: u64) -> Option<u64> {
    (expires_in_secs.saturating_mul(1000) > REFRESH_MARGIN_MS)
        .then(|| expires_in_secs.min(MAX_LIFETIME_SECS) * 1000)
}

fn parse_response(value: &Value) -> Response {
    let access = match non_empty(value.get("access_token")) {
        None => Err("the Claude Code token refresh response is missing required fields"),
        Some(access) => Ok(access),
    };
    let lifetime_ms = match value.get("expires_in").and_then(Value::as_u64) {
        None => Err("the Claude Code token refresh response is missing required fields"),
        Some(seconds) => lifetime_ms(seconds).ok_or(
            "the Claude Code token refresh response granted a token lifetime too short to use",
        ),
    };
    Response {
        access,
        refresh: non_empty(value.get("refresh_token")),
        lifetime_ms,
        scope: non_empty(value.get("scope")),
    }
}

/// Update the credential fields in place, preserving every other key and the document's
/// shape. `usable` is the new access token and its expiry; without one the old access token
/// stays and is marked expired, so the next access refreshes with the rotated refresh token.
fn merge_document(
    existing: &[u8],
    stored: Option<&Stored>,
    response: &Response,
    usable: Option<(&str, u64)>,
) -> Result<Vec<u8>, String> {
    let mut document = match serde_json::from_slice::<Value>(existing) {
        Ok(value) if value.is_object() => value,
        _ => json!({ "claudeAiOauth": {} }),
    };
    let root = document
        .as_object_mut()
        .expect("document is an object by construction");
    let target = if root.contains_key("claudeAiOauth") {
        root.get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| "the Claude Code claudeAiOauth field is not an object".to_string())?
    } else {
        root
    };
    match usable {
        Some((access, expires_ms)) => {
            target.insert("accessToken".to_string(), json!(access));
            target.insert("expiresAt".to_string(), json!(expires_ms));
        }
        None => {
            target.insert("expiresAt".to_string(), json!(0));
        }
    }
    // Keep the prior refresh token when the response rotates none (older servers), and keep
    // the existing scopes unless the response names new ones.
    let refresh = response
        .refresh
        .clone()
        .or_else(|| stored.and_then(|stored| stored.refresh.clone()));
    if let Some(refresh) = refresh {
        target.insert("refreshToken".to_string(), json!(refresh));
    }
    if let Some(scope) = &response.scope {
        let scopes: Vec<Value> = scope.split_whitespace().map(|scope| json!(scope)).collect();
        if !scopes.is_empty() {
            target.insert("scopes".to_string(), Value::Array(scopes));
        }
    }
    let mut encoded = serde_json::to_string_pretty(&document)
        .map_err(|_| "Claude Code credentials could not be encoded".to_string())?;
    encoded.push('\n');
    Ok(encoded.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_lifetime_is_refused_and_a_huge_one_capped() {
        assert_eq!(lifetime_ms(0), None);
        assert_eq!(lifetime_ms(300), None);
        assert_eq!(lifetime_ms(301), Some(301_000));
        assert_eq!(lifetime_ms(3600), Some(3_600_000));
        assert_eq!(lifetime_ms(u64::MAX), Some(366 * 24 * 3600 * 1000));
    }

    #[test]
    fn the_refresh_asks_for_the_scopes_the_login_records_else_the_defaults() {
        let request = |file: Option<&[u8]>| -> Value {
            let text = ClaudeCode.refresh_request("FAKE-REFRESH", file).unwrap();
            let request: Value = serde_json::from_str(&text).unwrap();
            serde_json::from_str(request["body"].as_str().unwrap()).unwrap()
        };
        let recorded = br#"{"claudeAiOauth":{"accessToken":"a","scopes":["user:inference"," "]}}"#;
        assert_eq!(request(Some(recorded))["scope"], "user:inference");
        assert_eq!(request(None)["scope"], DEFAULT_SCOPES);
        let blank = br#"{"accessToken":"a","scopes":[]}"#;
        assert_eq!(request(Some(blank))["scope"], DEFAULT_SCOPES);
    }

    #[test]
    fn a_merge_keeps_the_shape_and_marks_a_refused_token_expired() {
        let flat = br#"{"accessToken":"old","refreshToken":"r1","expiresAt":5,"extra":1}"#;
        let body =
            br#"{"access_token":"new","refresh_token":"r2","expires_in":3600,"scope":"a b"}"#;
        let merged: Value = serde_json::from_slice(
            &ClaudeCode
                .merge_login(flat, body, 1_000, 2_000, true)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(merged["accessToken"], "new");
        assert_eq!(merged["expiresAt"], 3_601_000);
        assert_eq!(merged["refreshToken"], "r2");
        assert_eq!(merged["scopes"], json!(["a", "b"]));
        assert_eq!(merged["extra"], 1);
        let refused: Value = serde_json::from_slice(
            &ClaudeCode
                .merge_login(flat, body, 1_000, 2_000, false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(refused["accessToken"], "old");
        assert_eq!(refused["expiresAt"], 0);
        assert_eq!(refused["refreshToken"], "r2");
    }
}
