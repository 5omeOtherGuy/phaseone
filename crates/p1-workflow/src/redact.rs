//! Local masking at workflow observability and persistence boundaries. The sandbox
//! keeps no credential values in its logs or JSONL even when a script prints one.

pub(crate) fn text(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let key_end =
                (index + 1..bytes.len().min(index + 34)).find(|position| bytes[*position] == b'"');
            if let Some(key_end) = key_end {
                let key = &input[index + 1..key_end];
                if [
                    "api_key",
                    "password",
                    "secret",
                    "token",
                    "authorization",
                    "access",
                    "refresh",
                    "key",
                ]
                .contains(&key)
                {
                    let mut value_start = key_end + 1;
                    while bytes.get(value_start).is_some_and(u8::is_ascii_whitespace) {
                        value_start += 1;
                    }
                    if bytes.get(value_start) == Some(&b':') {
                        value_start += 1;
                        while bytes.get(value_start).is_some_and(u8::is_ascii_whitespace) {
                            value_start += 1;
                        }
                        if bytes.get(value_start) == Some(&b'"') {
                            value_start += 1;
                            let mut end = value_start;
                            while end < bytes.len() {
                                if bytes[end] == b'\\' {
                                    end = end.saturating_add(2);
                                    continue;
                                }
                                if bytes[end] == b'"' {
                                    break;
                                }
                                end += 1;
                            }
                            if end < bytes.len() && end > value_start {
                                output.push_str(&input[cursor..value_start]);
                                output.push_str("<redacted:credential>");
                                cursor = end;
                                index = end;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        let prefix = [
            b"sk-".as_slice(),
            b"ghp_",
            b"github_pat_",
            b"Bearer",
            b"Authorization:",
        ]
        .into_iter()
        .find(|prefix| {
            bytes[index..].get(..prefix.len()).is_some_and(|candidate| {
                if *prefix == b"Bearer" || *prefix == b"Authorization:" {
                    candidate.eq_ignore_ascii_case(prefix)
                } else {
                    candidate == *prefix
                }
            })
        });
        if let Some(prefix) = prefix {
            let mut start = index + prefix.len();
            if prefix == b"Authorization:" || prefix == b"Bearer" {
                if prefix == b"Bearer"
                    && !bytes
                        .get(start)
                        .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
                {
                    index += 1;
                    continue;
                }
                while bytes
                    .get(start)
                    .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
                {
                    start += 1;
                }
            }
            let end = bytes[start..]
                .iter()
                .position(|byte| {
                    !byte.is_ascii_alphanumeric()
                        && !matches!(*byte, b'_' | b'-' | b'.' | b'+' | b'/' | b'=' | b'~')
                })
                .map_or(bytes.len(), |offset| start + offset);
            let min_length = if prefix == b"Authorization:" || prefix == b"Bearer" {
                16
            } else {
                8
            };
            if end - start >= min_length {
                output.push_str(&input[cursor..index]);
                output.push_str("<redacted:credential>");
                cursor = end;
                index = end;
                continue;
            }
        }
        index += 1;
        while index < bytes.len() && !input.is_char_boundary(index) {
            index += 1;
        }
    }
    output.push_str(&input[cursor..]);
    output
}

pub(crate) fn value(input: &serde_json::Value) -> serde_json::Value {
    match input {
        serde_json::Value::String(text_value) => serde_json::Value::String(text(text_value)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(value).collect())
        }
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, item)| {
                    // A field names a credential only when its whole name is one: a
                    // `token_count` or `accessibility` field is data, not a secret.
                    let redacted = if credential_key(key) {
                        serde_json::Value::String("<redacted:credential>".into())
                    } else {
                        value(item)
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Whether the WHOLE key names a credential. Exact (case-insensitive) matching keeps
/// unrelated data fields such as `token_count`, `accessibility` or `secretary` intact;
/// a substring match used to rewrite them.
fn credential_key(key: &str) -> bool {
    [
        "api_key",
        "password",
        "secret",
        "token",
        "authorization",
        "access",
        "refresh",
        "key",
    ]
    .into_iter()
    .any(|name| key.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_like_value_does_not_survive_masking() {
        for prefix in [
            "sk-",
            "ghp_",
            "github_pat_",
            "Bearer ",
            "Authorization:",
            "authorization:\t",
            "bearer\t",
        ] {
            let secret = format!("{prefix}fakeexample123456789");
            assert!(!text(&format!("log: {secret} end")).contains(&secret));
        }
        for key in ["api_key", "access", "refresh", "token", "key", "password"] {
            let secret = format!("prefix {{\"{key}\":\"fakeexample123456789\"}} suffix");
            assert!(!text(&secret).contains("fakeexample123456789"));
        }
    }

    #[test]
    fn non_credential_fields_keep_their_values() {
        // None of these keys names a credential, so none may be rewritten: a substring
        // match used to redact `token_count`, `accessibility` and `secretary` too.
        let result = serde_json::json!({
            "token_count": 42,
            "accessibility": {"level": "full"},
            "secretary": "Jane",
            "tokens": ["a", "b"],
        });
        assert_eq!(value(&result), result);
        // Exact credential keys still mask their string values.
        let masked = value(&serde_json::json!({
            "api_key": "fakeexample123456789",
            "token": "fakeexample123456789",
        }));
        assert_eq!(
            masked["api_key"],
            serde_json::json!("<redacted:credential>")
        );
        assert_eq!(masked["token"], serde_json::json!("<redacted:credential>"));
    }
}
