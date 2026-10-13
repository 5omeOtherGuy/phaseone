//! The fixture plugin over the generic channel: one `handle` export, one match arm per event.
//!
//! `tool/execute` reads a wire tool call (`{"call_id", "name", "input": {"kind", "raw"}}`) and
//! answers a wire tool outcome; its raw input picks the mode:
//!
//! | input | answer |
//! |---|---|
//! | `echo:<text>` | ok outcome with `<text>`, as `p1-module-fixture` answers |
//! | `spawn:<command>` | `call("process", "spawn", ...)`; ok with its answer, error with the refusal |
//!
//! `context/prepare` reads `{"mode": ...}`: `inline` parses the `history` array of the payload
//! itself, `tail` reads the last 4 KiB of `open("history")`, `all` reads all of it in 64 KiB
//! chunks, `all-parse` reads all of it and parses it, `inline-unparsed` only measures the
//! payload it was handed.

use p1_bindings_channel::generated::Guest;
use p1_bindings_channel::generated::p1::channel::host;
use serde_json::{Value, json};

struct Fixture;

#[cfg(not(feature = "unknown-capability"))]
const MANIFEST: &str = r#"{"events":{"tool/execute":1,"tool/post-execute":1,"context/prepare":1},"capabilities":["process"]}"#;
#[cfg(feature = "unknown-capability")]
const MANIFEST: &str = r#"{"events":{"tool/execute":1},"capabilities":["process","teleport"]}"#;

const TAIL: u64 = 4 << 10;
const CHUNK: u64 = 64 << 10;

impl Guest for Fixture {
    fn manifest() -> String {
        MANIFEST.to_owned()
    }

    fn handle(event: String, payload: String) -> Option<String> {
        match event.as_str() {
            "tool/execute" => Some(execute(&payload)),
            "context/prepare" => Some(prepare(&payload)),
            "tool/post-execute" => Some(post_execute(&payload)),
            _ => None,
        }
    }
}

fn execute(payload: &str) -> String {
    let call: Value = match serde_json::from_str(payload) {
        Ok(call) => call,
        Err(error) => return outcome("error", &format!("unreadable call: {error}")),
    };
    let Some(raw) = call["input"]["raw"].as_str() else {
        return outcome("error", "the call has no input text");
    };
    if let Some(text) = raw.strip_prefix("echo:") {
        return outcome("ok", text);
    }
    if let Some(command) = raw.strip_prefix("spawn:") {
        let request = json!({ "command": command }).to_string();
        return match host::call("process", "spawn", &request) {
            Ok(answer) => outcome("ok", &answer),
            Err(refusal) => outcome("error", &format!("refused: {refusal}")),
        };
    }
    outcome("error", "unknown mode")
}

fn prepare(payload: &str) -> String {
    // The copy alone: the payload arrived, nothing reads it.
    if payload.starts_with(r#"{"mode":"inline-unparsed""#) {
        return json!({ "len": payload.len() }).to_string();
    }
    let request: Value = match serde_json::from_str(payload) {
        Ok(request) => request,
        Err(error) => return json!({ "error": error.to_string() }).to_string(),
    };
    let answer = match request["mode"].as_str() {
        Some("inline") => match request["history"].as_array() {
            Some(items) => json!({ "items": items.len() }),
            None => json!({ "error": "no history array" }),
        },
        Some("tail") => match host::open("history") {
            Ok(buffer) => {
                let len = buffer.len();
                let tail = buffer.read(len.saturating_sub(TAIL), TAIL);
                json!({ "len": len, "read": tail.len() })
            }
            Err(error) => json!({ "error": error }),
        },
        Some(mode @ ("all" | "all-parse")) => match host::open("history") {
            Ok(buffer) => {
                let bytes = read_all(&buffer);
                if mode == "all" {
                    json!({ "read": bytes.len() })
                } else {
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(Value::Array(items)) => json!({ "items": items.len() }),
                        _ => json!({ "error": "history is not a JSON array" }),
                    }
                }
            }
            Err(error) => json!({ "error": error }),
        },
        _ => json!({ "error": "unknown mode" }),
    };
    answer.to_string()
}

fn read_all(buffer: &host::Buffer) -> Vec<u8> {
    let len = buffer.len();
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    while (bytes.len() as u64) < len {
        let chunk = buffer.read(bytes.len() as u64, CHUNK);
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    bytes
}

fn outcome(status: &str, content: &str) -> String {
    json!({ "status": status, "content": content }).to_string()
}

p1_bindings_channel::generated::export!(Fixture);

/// `tool/post-execute`: `{"call": <wire tool call>, "outcome": <wire tool outcome>}`; the
/// plugin answers a note naming the call and how it ended.
fn post_execute(payload: &str) -> String {
    let event: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    json!({
        "note": format!(
            "{} ended {}",
            event["call"]["call_id"].as_str().unwrap_or("?"),
            event["outcome"]["status"].as_str().unwrap_or("?")
        )
    })
    .to_string()
}
