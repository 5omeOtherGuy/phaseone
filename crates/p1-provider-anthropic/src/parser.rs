//! The streaming SSE parser: a pure state machine from wire events to contract
//! stream events.
//!
//! Blocks are tracked by their wire `index`; the `block` field on a
//! `TextDelta`/`ReasoningDelta` is the index of that block in the FINAL item
//! (only blocks that end up in the item are counted). A tool call exists only at
//! its own `content_block_stop`, and its input is the concatenated
//! `partial_json` RAW TEXT — never parsed, repaired or defaulted.
//!
//! Nothing in this file copies response-body text into an error: an SSE `error`
//! event contributes only its enumerated `error.type`, and the free-text
//! `message` is used for classification alone.

use std::collections::{HashMap, HashSet};

use p1_contracts::history::{AssistantBlock, AssistantItem, Origin, ToolCall, ToolInput};
use p1_contracts::{
    CompletedResponse, Outcome, ProviderError, ProviderErrorKind, StopReason, StreamEvent, Usage,
};
use p1_provider_http::{ResponseParser, SseEvent, kind_for_status};
use serde_json::Value;

use crate::replay;

/// Parse one streaming Messages response into contract events.
pub struct AnthropicParser {
    origin_route: String,
    /// The CONFIGURED wire model, kept for the whole stream: `message_start` reports a
    /// dated alias of the model that was asked for, and replay is gated on origin
    /// equality, so keying on the echoed name would make a session's own thinking
    /// foreign to itself (ADR-0018, routes.md §A "Replay").
    origin_model: String,
    blocks: Vec<AssistantBlock>,
    open: HashMap<usize, OpenBlock>,
    stop: Option<StopReason>,
    usage: Option<Usage>,
    usage_seen: bool,
    message_stopped: bool,
    message_started: bool,
    pre_start_content: bool,
    closed: HashSet<usize>,
    decoded_bytes: usize,
    terminal: Option<Outcome>,
}

struct OpenBlock {
    final_index: usize,
    kind: OpenKind,
}

enum OpenKind {
    Text,
    Thinking {
        /// The signature parts received so far, in arrival order. The payload is
        /// re-encoded from it, so the codec stays the only writer of a payload.
        signature: String,
    },
    Redacted,
    Tool {
        call_id: String,
        name: String,
        partial_json: String,
    },
}

impl AnthropicParser {
    /// `origin_route` is the composed route's `Origin.route`; the model is the
    /// configured wire model, which every replay payload is tagged with.
    pub fn new(origin_route: &str, model: &str) -> Self {
        Self {
            origin_route: origin_route.to_string(),
            origin_model: model.to_string(),
            blocks: Vec::new(),
            open: HashMap::new(),
            stop: None,
            usage: None,
            usage_seen: false,
            message_stopped: false,
            message_started: false,
            pre_start_content: false,
            closed: HashSet::new(),
            decoded_bytes: 0,
            terminal: None,
        }
    }

    fn origin(&self) -> Origin {
        Origin {
            route: self.origin_route.clone(),
            model: self.origin_model.clone(),
        }
    }

    /// Queue the single terminal outcome. Any later event is ignored, so the
    /// stream can never carry two terminal events.
    fn fail(&mut self, kind: ProviderErrorKind, message: impl Into<String>) {
        if self.terminal.is_none() {
            self.terminal = Some(Outcome::Failed(ProviderError::new(kind, message)));
        }
    }

    fn complete(&mut self) {
        if self.terminal.is_some() {
            return;
        }
        let mut ids = HashSet::new();
        if self.blocks.iter().any(|block| matches!(block, AssistantBlock::ToolCall(call) if call.call_id.is_empty() || call.name.is_empty() || !ids.insert(call.call_id.clone()))) {
            self.fail(ProviderErrorKind::Protocol, "incomplete or duplicate tool identity");
            return;
        }
        let stop = self.stop.expect("checked at message_stop");
        if matches!(
            stop,
            StopReason::MaxOutputTokens | StopReason::Refusal | StopReason::ContextWindowExceeded
        ) {
            self.blocks
                .retain(|block| !matches!(block, AssistantBlock::ToolCall(_)));
        } else if self
            .blocks
            .iter()
            .any(|block| matches!(block, AssistantBlock::ToolCall(_)))
            && stop != StopReason::ToolUse
        {
            self.fail(
                ProviderErrorKind::Protocol,
                "tool calls without tool finish reason",
            );
            return;
        }
        let completed = CompletedResponse {
            item: AssistantItem {
                origin: self.origin(),
                blocks: std::mem::take(&mut self.blocks),
            },
            stop,
            usage: if self.usage_seen { self.usage } else { None },
        };
        self.terminal = Some(Outcome::Completed(completed));
    }

    /// Merge a `usage` object field-by-field. An absent field keeps its previous
    /// value (message_delta overrides message_start); a field never seen stays
    /// `None`, never zero.
    fn merge_usage(&mut self, usage: &Value) {
        self.usage_seen = true;
        let merged = self.usage.get_or_insert_with(Usage::default);
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            merged.input_uncached = Some(value);
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            merged.cache_read = Some(value);
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            merged.cache_write = Some(value);
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            merged.output = Some(value);
        }
        // This route never reports reasoning tokens or cost.
    }

    fn on_content_block_start(&mut self, value: &Value) {
        let Some(index) = block_index(value) else {
            self.fail(ProviderErrorKind::Protocol, "invalid block index");
            return;
        };
        if self.open.contains_key(&index) || self.closed.contains(&index) {
            self.fail(ProviderErrorKind::Protocol, "duplicate block start");
            return;
        }
        let block = value.get("content_block");
        let final_index = self.blocks.len();
        match block
            .and_then(|block| block.get("type"))
            .and_then(Value::as_str)
        {
            Some("text") => {
                let text = str_field(block, "text");
                self.blocks.push(AssistantBlock::Text { text });
                self.open.insert(
                    index,
                    OpenBlock {
                        final_index,
                        kind: OpenKind::Text,
                    },
                );
            }
            Some("thinking") => {
                let text = str_field(block, "thinking");
                let replay = replay::encode(
                    &self.origin(),
                    replay::WireBlock::Thinking { signature: "" },
                );
                self.blocks.push(AssistantBlock::Reasoning {
                    text,
                    replay: Some(replay),
                });
                self.open.insert(
                    index,
                    OpenBlock {
                        final_index,
                        kind: OpenKind::Thinking {
                            signature: String::new(),
                        },
                    },
                );
            }
            Some("redacted_thinking") => {
                let data = str_field(block, "data");
                let replay =
                    replay::encode(&self.origin(), replay::WireBlock::Redacted { data: &data });
                self.blocks.push(AssistantBlock::Reasoning {
                    text: String::new(),
                    replay: Some(replay),
                });
                self.open.insert(
                    index,
                    OpenBlock {
                        final_index,
                        kind: OpenKind::Redacted,
                    },
                );
            }
            Some("tool_use") => {
                let call_id = str_field(block, "id");
                let name = str_field(block, "name");
                // A placeholder keeps the block's position (and therefore every
                // later block's final index) in the item; it is replaced with the
                // complete call at `content_block_stop`.
                self.blocks.push(AssistantBlock::ToolCall(ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    input: ToolInput::Json(String::new()),
                }));
                self.open.insert(
                    index,
                    OpenBlock {
                        final_index,
                        kind: OpenKind::Tool {
                            call_id,
                            name,
                            partial_json: String::new(),
                        },
                    },
                );
            }
            _ => self.fail(ProviderErrorKind::Protocol, "unknown content block type"),
        }
    }

    fn on_content_block_delta(&mut self, value: &Value, events: &mut Vec<StreamEvent>) {
        let Some(index) = block_index(value) else {
            self.fail(ProviderErrorKind::Protocol, "invalid block index");
            return;
        };
        let Some(delta) = value.get("delta") else {
            self.fail(ProviderErrorKind::Protocol, "missing block delta");
            return;
        };
        let valid = matches!(
            (
                self.open.get(&index).map(|b| &b.kind),
                delta.get("type").and_then(Value::as_str)
            ),
            (Some(OpenKind::Text), Some("text_delta"))
                | (Some(OpenKind::Tool { .. }), Some("input_json_delta"))
                | (
                    Some(OpenKind::Thinking { .. }),
                    Some("thinking_delta" | "signature_delta")
                )
        );
        if !valid
            || !match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => delta.get("text").is_some_and(Value::is_string),
                Some("input_json_delta") => delta.get("partial_json").is_some_and(Value::is_string),
                Some("thinking_delta") => delta.get("thinking").is_some_and(Value::is_string),
                Some("signature_delta") => delta.get("signature").is_some_and(Value::is_string),
                _ => false,
            }
        {
            self.fail(ProviderErrorKind::Protocol, "unmatched block delta");
            return;
        }
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let Some(text) = delta.get("text").and_then(Value::as_str) else {
                    return;
                };
                let Some(open) = self.open.get(&index) else {
                    return;
                };
                if !matches!(open.kind, OpenKind::Text) {
                    return;
                }
                if let AssistantBlock::Text { text: buffer } = &mut self.blocks[open.final_index] {
                    buffer.push_str(text);
                }
                events.push(StreamEvent::TextDelta {
                    block: open.final_index,
                    text: text.to_string(),
                });
            }
            Some("input_json_delta") => {
                let Some(partial) = delta.get("partial_json").and_then(Value::as_str) else {
                    return;
                };
                let Some(open) = self.open.get_mut(&index) else {
                    return;
                };
                if let OpenKind::Tool {
                    call_id,
                    name,
                    partial_json,
                } = &mut open.kind
                {
                    partial_json.push_str(partial);
                    events.push(StreamEvent::ToolInputDelta {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        text: partial.to_string(),
                    });
                }
            }
            Some("thinking_delta") => {
                let Some(thinking) = delta.get("thinking").and_then(Value::as_str) else {
                    return;
                };
                let Some(open) = self.open.get(&index) else {
                    return;
                };
                // Redacted thinking is never streamed as a delta: only a visible
                // thinking block can produce ReasoningDelta.
                if !matches!(open.kind, OpenKind::Thinking { .. }) {
                    return;
                }
                if let AssistantBlock::Reasoning { text, .. } = &mut self.blocks[open.final_index] {
                    text.push_str(thinking);
                }
                if !thinking.is_empty() {
                    events.push(StreamEvent::ReasoningDelta {
                        block: open.final_index,
                        text: thinking.to_string(),
                    });
                }
            }
            Some("signature_delta") => {
                let Some(signature) = delta.get("signature").and_then(Value::as_str) else {
                    return;
                };
                let origin = self.origin();
                let Some(open) = self.open.get_mut(&index) else {
                    return;
                };
                let OpenBlock { final_index, kind } = open;
                // The signature arrives in parts and only a visible thinking block
                // carries one.
                let OpenKind::Thinking { signature: parts } = kind else {
                    return;
                };
                parts.push_str(signature);
                if let AssistantBlock::Reasoning {
                    replay: Some(replay),
                    ..
                } = &mut self.blocks[*final_index]
                {
                    *replay =
                        replay::encode(&origin, replay::WireBlock::Thinking { signature: parts });
                }
            }
            _ => {}
        }
    }

    fn on_content_block_stop(&mut self, value: &Value) {
        let Some(index) = block_index(value) else {
            self.fail(ProviderErrorKind::Protocol, "invalid block index");
            return;
        };
        let Some(open) = self.open.remove(&index) else {
            self.fail(ProviderErrorKind::Protocol, "unmatched block stop");
            return;
        };
        self.closed.insert(index);
        if let OpenKind::Tool {
            call_id,
            name,
            partial_json,
        } = open.kind
        {
            // Empty buffer means the call takes no arguments; the raw text is
            // otherwise preserved byte-exact, including invalid JSON.
            let raw = if partial_json.is_empty() {
                "{}".to_string()
            } else {
                partial_json
            };
            self.blocks[open.final_index] = AssistantBlock::ToolCall(ToolCall {
                call_id,
                name,
                input: ToolInput::Json(raw),
            });
        }
    }

    fn on_message_start(&mut self, value: &Value) {
        let message = value.get("message");
        // The item's origin is ALWAYS the configured route + model, never the model
        // name echoed by the response: responses report dated aliases of the model
        // that was asked for, and replay data is gated on origin equality — keying
        // on the echoed name would make every thinking block "foreign" on the next
        // request (routes.md §A Replay, ruling in providers.md).
        if let Some(usage) = message.and_then(|message| message.get("usage")) {
            self.merge_usage(usage);
        }
    }
}

impl ResponseParser for AnthropicParser {
    fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent> {
        if self.terminal.is_some() {
            return Vec::new();
        }
        self.decoded_bytes = self.decoded_bytes.saturating_add(event.data.len());
        if event.data.len() > 1_048_576
            || self.decoded_bytes > 16_777_216
            || self.blocks.len() > 4096
            || self.open.len() > 4096
        {
            self.fail(
                ProviderErrorKind::Protocol,
                "provider response exceeds size limit",
            );
            return vec![StreamEvent::Finished(self.terminal.clone().unwrap())];
        }
        let mut events = Vec::new();

        // A `[DONE]` sentinel is not part of this route's protocol; tolerate it
        // rather than turning a stray keep-alive into a protocol failure.
        if event.data.trim() == "[DONE]" {
            return events;
        }

        let value: Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(_) => {
                self.fail(
                    ProviderErrorKind::Protocol,
                    "provider sent an SSE event that is not valid JSON",
                );
                events.push(StreamEvent::Finished(self.terminal.clone().unwrap()));
                return events;
            }
        };

        match value.get("type").and_then(Value::as_str) {
            Some("ping") => events.push(StreamEvent::Activity),
            Some("message_start") => {
                if self.pre_start_content {
                    self.fail(ProviderErrorKind::Protocol, "content before message start");
                } else if self.message_started {
                    self.fail(ProviderErrorKind::Protocol, "duplicate message start");
                } else if !value.get("message").is_some_and(Value::is_object) {
                    self.fail(ProviderErrorKind::Protocol, "message start without message");
                } else {
                    self.message_started = true;
                    self.on_message_start(&value);
                }
            }
            // A broken stream can start with content and then disconnect. Keep its
            // visible delta so the frozen no-retry-after-output contract still
            // reports Transport on EOF; message_stop still cannot complete it.
            Some("message_delta" | "message_stop") if !self.message_started => {
                self.fail(ProviderErrorKind::Protocol, "event before message start");
            }
            Some("content_block_start") => {
                self.pre_start_content |= !self.message_started;
                self.on_content_block_start(&value);
            }
            Some("content_block_delta") => {
                self.pre_start_content |= !self.message_started;
                self.on_content_block_delta(&value, &mut events);
            }
            Some("content_block_stop") => {
                self.pre_start_content |= !self.message_started;
                self.on_content_block_stop(&value);
            }
            Some("message_delta") => {
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop = Some(map_stop_reason(reason));
                }
                if let Some(usage) = value.get("usage") {
                    self.merge_usage(usage);
                }
            }
            Some("message_stop") => {
                self.message_stopped = true;
                if self.stop.is_none() {
                    self.fail(ProviderErrorKind::Protocol, "message has no stop reason");
                } else if self.open.is_empty() {
                    self.complete();
                } else {
                    // routes.md: a stop with a block still open is a broken
                    // stream, never an implicit completion.
                    self.fail(
                        ProviderErrorKind::Transport,
                        "stream ended without a terminal event",
                    );
                }
            }
            Some("error") => {
                let error = value.get("error");
                let error_type = error
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                let message = error
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let kind = classify_error(error_type, message);
                // Only the enumerated type is surfaced; the free-text message is
                // classification input and is never copied.
                let label = displayed_error_type(error_type);
                self.fail(kind, format!("provider error: {label}"));
            }
            // Unknown event types are ignored for forward compatibility.
            _ => {}
        }

        if let Some(outcome) = self.terminal.clone() {
            events.push(StreamEvent::Finished(outcome));
        }
        events
    }

    fn on_end(&mut self) -> Outcome {
        if let Some(outcome) = self.terminal.clone() {
            return outcome;
        }
        let outcome = Outcome::Failed(ProviderError::new(
            ProviderErrorKind::Transport,
            "stream ended without a terminal event",
        ));
        self.terminal = Some(outcome.clone());
        outcome
    }

    fn on_http_error(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> ProviderError {
        let error_type = extract_error_type(body);
        let message_text = extract_error_message(body).unwrap_or_default();
        let context_window = error_type.as_deref().is_some_and(is_context_window_type)
            || is_context_window(&message_text);

        let kind = if matches!(status, 400 | 413 | 422) && context_window {
            ProviderErrorKind::ContextWindowExceeded
        } else {
            kind_for_status(status).unwrap_or(ProviderErrorKind::InvalidRequest)
        };

        // Status, enumerated error type and the request id only: never body text.
        let mut message = format!("http {status}");
        if let Some(error_type) = &error_type {
            message.push(' ');
            message.push_str(displayed_error_type(error_type));
        }
        if let Some(id) = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("request-id"))
            .and_then(|(_, value)| safe_request_id(value))
        {
            message.push_str(&format!(" request-id={id}"));
        }
        ProviderError::new(kind, message)
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxOutputTokens,
        "model_context_window_exceeded" => StopReason::ContextWindowExceeded,
        "refusal" => StopReason::Refusal,
        "pause_turn" => StopReason::Paused,
        // `stop_sequence` and every unknown value share the catch-all, matching
        // the contract's `Other`.
        _ => StopReason::Other,
    }
}

/// Only vendor-defined error labels, never arbitrary response text.
fn displayed_error_type(value: &str) -> &'static str {
    match value {
        "overloaded_error" => "overloaded_error",
        "api_error" => "api_error",
        "rate_limit_error" => "rate_limit_error",
        "authentication_error" => "authentication_error",
        "permission_error" => "permission_error",
        "invalid_request_error" => "invalid_request_error",
        "context_window_exceeded" => "context_window_exceeded",
        _ => "unknown",
    }
}

/// The frozen HTTP-error contract exposes `req_*` request IDs. Refuse arbitrary
/// token-shaped header values and malformed IDs; never expose other headers.
fn safe_request_id(value: &str) -> Option<&str> {
    let suffix = value.strip_prefix("req_")?;
    (value.len() <= 64
        && !suffix.is_empty()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(value)
}

fn classify_error(error_type: &str, message: &str) -> ProviderErrorKind {
    match error_type {
        "overloaded_error" | "api_error" => ProviderErrorKind::Transport,
        "rate_limit_error" => ProviderErrorKind::RateLimited,
        "authentication_error" | "permission_error" => ProviderErrorKind::Authentication,
        "invalid_request_error" if is_context_window(message) => {
            ProviderErrorKind::ContextWindowExceeded
        }
        "invalid_request_error" => ProviderErrorKind::InvalidRequest,
        _ => ProviderErrorKind::Transport,
    }
}

/// Heuristic classification of a free-text provider message. The text is read,
/// never copied.
fn is_context_window(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("context window") || text.contains("too long")
}

fn is_context_window_type(error_type: &str) -> bool {
    error_type.contains("context_window")
}

/// Extract `error.type` from an error body. The body is classification input:
/// on any parse failure (including a non-JSON body) the result is `None`.
fn extract_error_type(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_error_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn block_index(value: &Value) -> Option<usize> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

fn str_field(value: Option<&Value>, field: &str) -> String {
    value
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use serde_json::json;

    fn send(parser: &mut AnthropicParser, value: Value) -> Vec<StreamEvent> {
        parser.on_event(SseEvent {
            event: None,
            data: value.to_string(),
        })
    }
    fn parser() -> AnthropicParser {
        AnthropicParser::new("anthropic", "model")
    }
    fn failed(events: &[StreamEvent]) {
        assert!(
            matches!(events.last(), Some(StreamEvent::Finished(Outcome::Failed(error))) if error.kind == ProviderErrorKind::Protocol),
            "{events:?}"
        );
    }
    fn start(p: &mut AnthropicParser) {
        send(p, json!({"type":"message_start","message":{}}));
    }
    fn block(p: &mut AnthropicParser, index: usize, id: &str, name: &str) {
        send(
            p,
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name}}),
        );
        send(p, json!({"type":"content_block_stop","index":index}));
    }
    #[test]
    fn message_start_requires_a_message_object() {
        for event in [
            json!({"type":"message_start"}),
            json!({"type":"message_start","message":null}),
            json!({"type":"message_start","message":"x"}),
            json!({"type":"message_start","message":[]}),
        ] {
            failed(&send(&mut parser(), event));
        }
    }
    #[test]
    fn malformed_lifecycle_and_blocks_fail() {
        for event in [
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"content_block_stop","index":0}),
        ] {
            failed(&send(&mut parser(), event));
        }
        let mut p = parser();
        start(&mut p);
        failed(&send(&mut p, json!({"type":"message_start"})));
        for bad in [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"unknown"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","content_block":{"type":"text"}}),
        ] {
            let mut p = parser();
            start(&mut p);
            failed(&send(&mut p, bad));
        }
        let mut p = parser();
        start(&mut p);
        send(
            &mut p,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        );
        failed(&send(
            &mut p,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
        ));
    }
    #[test]
    fn pre_start_content_cannot_become_a_completed_message() {
        for block in [
            json!({"type":"text","text":""}),
            json!({"type":"tool_use","id":"c","name":"read"}),
        ] {
            let mut p = parser();
            send(
                &mut p,
                json!({"type":"content_block_start","index":0,"content_block":block}),
            );
            let delta = if block["type"] == "text" {
                json!({"type":"text_delta","text":"visible"})
            } else {
                json!({"type":"input_json_delta","partial_json":"{}"})
            };
            let visible = send(
                &mut p,
                json!({"type":"content_block_delta","index":0,"delta":delta}),
            );
            assert!(!visible.is_empty());
            send(&mut p, json!({"type":"content_block_stop","index":0}));
            failed(&send(&mut p, json!({"type":"message_start","message":{}})));
            assert!(send(&mut p, json!({"type":"message_stop"})).is_empty());
        }
        let mut p = parser();
        send(
            &mut p,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
        );
        assert!(
            matches!(p.on_end(), Outcome::Failed(error) if error.kind == ProviderErrorKind::Transport)
        );
    }
    #[test]
    fn stop_reason_and_tool_identity_are_required() {
        let mut p = parser();
        start(&mut p);
        failed(&send(&mut p, json!({"type":"message_stop"})));
        let mut p = parser();
        start(&mut p);
        send(
            &mut p,
            json!({"type":"message_delta","delta":{"stop_reason":"future_reason"}}),
        );
        assert!(
            matches!(send(&mut p, json!({"type":"message_stop"})).last(), Some(StreamEvent::Finished(Outcome::Completed(c))) if c.stop == StopReason::Other)
        );
        for (id, name) in [("", "read"), ("c", "")] {
            let mut p = parser();
            start(&mut p);
            block(&mut p, 0, id, name);
            send(
                &mut p,
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
            );
            failed(&send(&mut p, json!({"type":"message_stop"})));
        }
        let mut p = parser();
        start(&mut p);
        block(&mut p, 0, "c", "read");
        block(&mut p, 1, "c", "read");
        send(
            &mut p,
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        );
        failed(&send(&mut p, json!({"type":"message_stop"})));
    }
    #[test]
    fn truncated_or_refused_calls_are_not_executable() {
        for reason in ["max_tokens", "refusal", "model_context_window_exceeded"] {
            let mut p = parser();
            start(&mut p);
            block(&mut p, 0, "c", "read");
            send(
                &mut p,
                json!({"type":"message_delta","delta":{"stop_reason":reason}}),
            );
            assert!(
                matches!(send(&mut p, json!({"type":"message_stop"})).last(), Some(StreamEvent::Finished(Outcome::Completed(c))) if c.item.tool_calls().next().is_none())
            );
        }
    }
    #[test]
    fn hostile_error_labels_and_header_are_not_reflected() {
        let credential_shaped = "AbCdEfGhIjKlMnOpQrStUvWxYz012345";
        let mut p = parser();
        let events = send(
            &mut p,
            json!({"type":"error","error":{"type":credential_shaped}}),
        );
        assert!(!format!("{events:?}").contains(credential_shaped));
        let error = parser().on_http_error(
            400,
            &[("request-id".into(), credential_shaped.into())],
            json!({"error":{"type":credential_shaped}})
                .to_string()
                .as_bytes(),
        );
        assert!(!error.message.contains(credential_shaped));
        let mut p = parser();
        let events = send(
            &mut p,
            json!({"type":"error","error":{"type":"sentinel secret\nvalue"}}),
        );
        assert!(!format!("{events:?}").contains("sentinel"));
        let error = parser().on_http_error(
            400,
            &[("request-id".into(), "sentinel secret".into())],
            br#"{"error":{"type":"sentinel secret"}}"#,
        );
        assert!(!error.message.contains("sentinel"));
    }
}
