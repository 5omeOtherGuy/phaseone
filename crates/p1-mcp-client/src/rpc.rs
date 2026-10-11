//! JSON-RPC 2.0 framing shared by both transports: building messages, classifying what the
//! server sent, and the one answer the client gives to a server's own request.

use p1_contracts::mcp::McpError;
use serde_json::{Value, json};

/// The JSON-RPC error code for a method the client does not implement.
const METHOD_NOT_FOUND: i64 = -32601;

/// A request the client sends.
pub(crate) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// A notification the client sends.
pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// One message the server sent.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// The answer to the client's request `id`.
    Response {
        id: u64,
        result: Result<Value, McpError>,
    },
    /// A request from the server; it needs [`answer`].
    Request { id: Value, method: String },
    /// A notification from the server: logging, progress, list changes. This slice reads
    /// none of them.
    Notification,
    /// Not a JSON-RPC 2.0 message this client can place.
    Invalid(String),
}

/// Classify one decoded message.
pub(crate) fn classify(message: Value) -> Incoming {
    let Value::Object(mut fields) = message else {
        return Incoming::Invalid("a message is not a JSON object".into());
    };
    if fields.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Incoming::Invalid("a message is not JSON-RPC 2.0".into());
    }
    let method = fields
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (fields.remove("id"), method) {
        (Some(id), Some(method)) if !id.is_null() => Incoming::Request { id, method },
        (None, Some(_)) => Incoming::Notification,
        (Some(id), None) => {
            // The client only ever sends numeric ids, so any other id answers nothing it
            // asked.
            let Some(id) = id.as_u64() else {
                return Incoming::Invalid("a response carries an id the client never sent".into());
            };
            let result = match (fields.remove("result"), fields.remove("error")) {
                (Some(result), None) => Ok(result),
                (None, Some(error)) => Err(server_error(&error)),
                _ => Err(McpError::Protocol(
                    "a response carries neither exactly a result nor an error".into(),
                )),
            };
            Incoming::Response { id, result }
        }
        _ => Incoming::Invalid(
            "a message is neither a request, a notification nor a response".into(),
        ),
    }
}

/// The client's answer to a server request: `ping` is answered, everything else (sampling,
/// roots, elicitation) is not offered by this client.
pub(crate) fn answer(id: Value, method: &str) -> Value {
    if method == "ping" {
        json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": METHOD_NOT_FOUND, "message": "method not offered by this client" },
        })
    }
}

fn server_error(error: &Value) -> McpError {
    McpError::Server {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message")
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_with_a_result_answers_its_id() {
        let incoming = classify(json!({"jsonrpc": "2.0", "id": 3, "result": {"ok": true}}));
        let Incoming::Response { id, result } = incoming else {
            panic!("not a response: {incoming:?}");
        };
        assert_eq!(id, 3);
        assert_eq!(result.unwrap(), json!({"ok": true}));
    }

    #[test]
    fn an_error_response_is_a_server_error() {
        let incoming = classify(json!({
            "jsonrpc": "2.0", "id": 4, "error": {"code": -32602, "message": "bad params"}
        }));
        let Incoming::Response { result, .. } = incoming else {
            panic!("not a response");
        };
        assert_eq!(
            result.unwrap_err(),
            McpError::Server {
                code: -32602,
                message: "bad params".into()
            }
        );
    }

    #[test]
    fn requests_notifications_and_noise_are_told_apart() {
        assert!(matches!(
            classify(json!({"jsonrpc": "2.0", "id": "s1", "method": "ping"})),
            Incoming::Request { .. }
        ));
        assert!(matches!(
            classify(json!({"jsonrpc": "2.0", "method": "notifications/message"})),
            Incoming::Notification
        ));
        assert!(matches!(classify(json!([1, 2])), Incoming::Invalid(_)));
        assert!(matches!(
            classify(json!({"id": 1, "result": {}})),
            Incoming::Invalid(_)
        ));
        assert!(matches!(
            classify(json!({"jsonrpc": "2.0", "id": "x", "result": {}})),
            Incoming::Invalid(_)
        ));
    }

    #[test]
    fn only_ping_is_answered_with_a_result() {
        assert_eq!(answer(json!(7), "ping")["result"], json!({}));
        assert_eq!(
            answer(json!(8), "sampling/createMessage")["error"]["code"],
            json!(METHOD_NOT_FOUND)
        );
    }
}
