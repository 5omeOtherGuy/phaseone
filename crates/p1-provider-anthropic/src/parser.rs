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

use std::collections::HashMap;

use p1_contracts::history::{
    AssistantBlock, AssistantItem, Origin, ReplayData, ToolCall, ToolInput,
};
use p1_contracts::{
    CompletedResponse, Outcome, ProviderError, ProviderErrorKind, StopReason, StreamEvent, Usage,
};
use p1_provider_http::{ResponseParser, SseEvent, kind_for_status};
use serde_json::{Value, json};

/// Parse one streaming Messages response into contract events.
pub struct AnthropicParser {
    origin_route: String,
    /// Starts as the requested model and is re-keyed by `message_start` to the
    /// RESPONSE model, so replay data carries the model that actually produced it.
    origin_model: String,
    blocks: Vec<AssistantBlock>,
    open: HashMap<usize, OpenBlock>,
    stop: Option<StopReason>,
    usage: Option<Usage>,
    usage_seen: bool,
    message_stopped: bool,
    terminal: Option<Outcome>,
}

struct OpenBlock {
    final_index: usize,
    kind: OpenKind,
}

enum OpenKind {
    Text,
    Thinking,
    Redacted,
    Tool {
        call_id: String,
        name: String,
        partial_json: String,
    },
}

impl AnthropicParser {
    /// `origin_route` is the composed route's `Origin.route`; the model is the
    /// configured wire model that `message_start` re-keys to the RESPONSE model.
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
        let completed = CompletedResponse {
            item: AssistantItem {
                origin: self.origin(),
                blocks: std::mem::take(&mut self.blocks),
            },
            stop: self.stop.unwrap_or(StopReason::Other),
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
        let index = block_index(value);
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
                let replay = ReplayData {
                    origin: self.origin(),
                    version: 1,
                    payload: json!({ "type": "thinking", "signature": "" }),
                };
                self.blocks.push(AssistantBlock::Reasoning {
                    text,
                    replay: Some(replay),
                });
                self.open.insert(
                    index,
                    OpenBlock {
                        final_index,
                        kind: OpenKind::Thinking,
                    },
                );
            }
            Some("redacted_thinking") => {
                let data = str_field(block, "data");
                let replay = ReplayData {
                    origin: self.origin(),
                    version: 1,
                    payload: json!({ "type": "redacted_thinking", "data": data }),
                };
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
            // An unknown block type ends up in no item, so it owns no index.
            _ => {}
        }
    }

    fn on_content_block_delta(&mut self, value: &Value, events: &mut Vec<StreamEvent>) {
        let index = block_index(value);
        let Some(delta) = value.get("delta") else {
            return;
        };
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
                if !matches!(open.kind, OpenKind::Thinking) {
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
                let Some(open) = self.open.get(&index) else {
                    return;
                };
                if !matches!(open.kind, OpenKind::Thinking) {
                    return;
                }
                if let AssistantBlock::Reasoning {
                    replay: Some(replay),
                    ..
                } = &mut self.blocks[open.final_index]
                {
                    let existing = replay
                        .payload
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    replay.payload["signature"] = json!(format!("{existing}{signature}"));
                }
            }
            _ => {}
        }
    }

    fn on_content_block_stop(&mut self, value: &Value) {
        let index = block_index(value);
        let Some(open) = self.open.remove(&index) else {
            return;
        };
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
            Some("message_start") => self.on_message_start(&value),
            Some("content_block_start") => self.on_content_block_start(&value),
            Some("content_block_delta") => self.on_content_block_delta(&value, &mut events),
            Some("content_block_stop") => self.on_content_block_stop(&value),
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
                if self.open.is_empty() {
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
                self.fail(kind, format!("provider error: {error_type}"));
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
            message.push_str(error_type);
        }
        if let Some(request_id) = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("request-id"))
            .map(|(_, value)| value)
        {
            message.push_str(&format!(" request-id={request_id}"));
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

fn block_index(value: &Value) -> usize {
    value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize
}

fn str_field(value: Option<&Value>, field: &str) -> String {
    value
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
