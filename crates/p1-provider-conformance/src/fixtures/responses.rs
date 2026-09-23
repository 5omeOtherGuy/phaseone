//! Hand-written, real-shaped Codex Responses SSE transcripts, shared publicly.
//!
//! These are the nine route fixtures the shared conformance suite parameterises
//! over (`docs/design/providers.md` `RouteFixtures`). They contain no captured
//! authenticated traffic and no credentials. `response.completed` deliberately
//! omits `model` so the parser's origin falls back to the configured model and
//! replay round-trips work regardless of the model a test is built with.
#![allow(dead_code)]

/// Text "Hello" + " world", usage present.
pub const TEXT_TURN: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_text"}}

event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hello"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":" world"}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello world"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_text","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":5}}}}

data: [DONE]

"#;

/// Text, then ONE call: id "call_1", name "read", args {"path":"a.txt"}.
pub const TOOL_CALL_TURN: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_tool"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"I will read it."}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"I will read it."}]}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"call_1","delta":"{\"path\":\"a"}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_tool"}}

"#;

/// "call_1" read, "call_2" grep — order must be preserved.
pub const TWO_TOOL_CALLS: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_two"}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":1,"item":{"id":"fc_2","type":"function_call","call_id":"call_2","name":"grep","arguments":"{\"pattern\":\"needle\"}"}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_two"}}

"#;

/// The stream ends in the middle of a call's arguments: a failure, not a turn.
pub const TRUNCATED_TOOL_CALL: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_truncated"}}

event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":""}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"call_1","delta":"{\"path\":\"a"}
"#;

/// A COMPLETE call whose arguments are not valid JSON.
pub const INVALID_TOOL_JSON: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_invalid"}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\": "}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_invalid"}}

"#;

/// A provider-side error event mid-stream; the message must stay private.
pub const ERROR_EVENT: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_error"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"partial"}

event: response.failed
data: {"type":"response.failed","response":{"error":{"code":"invalid_request_error","message":"SENTINEL-BODY"}}}

"#;

/// Completes without any usage fields.
pub const NO_USAGE: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_no_usage"}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_no_usage"}}

"#;

/// Reasoning with replay data, then text.
pub const REASONING_TURN: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_reasoning"}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","encrypted_content":"enc-1","summary":[{"type":"summary_text","text":"first part"},{"type":"summary_text","text":"second part"}]}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":1,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_reasoning"}}

"#;

/// A complete turn followed by stray events after the terminal.
pub const EVENTS_AFTER_TERMINAL: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_after"}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_after"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"stray"}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_second"}}

data: [DONE]

"#;
