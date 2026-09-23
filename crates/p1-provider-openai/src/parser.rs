//! The Codex Responses SSE state machine.
//!
//! Pure and synchronous: [`SseEvent`]s in, contract [`StreamEvent`]s out. The
//! shared [`ResponseParser`] contract means the retrying driver owns retries,
//! cancellation and terminal-event bookkeeping. Wire facts: `routes.md` §B.

use p1_contracts::history::{
    AssistantBlock, AssistantItem, Origin, ReplayData, ToolCall, ToolInput,
};
use p1_contracts::{
    CompletedResponse, Outcome, ProviderError, ProviderErrorKind, StopReason, StreamEvent, Usage,
    serde_json,
};
use p1_provider_http::{ResponseParser, SseEvent, http_error_code, kind_for_status, safe_code};
use serde_json::Value;

/// Route-native parser. One instance per request attempt.
pub(crate) struct CodexResponseParser {
    route: String,
    model: String,
    /// Completed blocks in arrival order.
    blocks: Vec<AssistantBlock>,
    /// Set once a terminal outcome has been produced; later events are ignored.
    terminal: Option<Outcome>,
    /// Remembered for diagnostics only; the contract has no response-id field.
    #[allow(dead_code)]
    response_id: Option<String>,
    /// Model-facing names observed when function-call output items are announced.
    call_names: std::collections::HashMap<String, String>,
    /// Whether a reasoning summary delta has been emitted for the item currently
    /// being produced, used to insert a section break on a later summary part.
    reasoning_emitted_in_item: bool,
}

impl CodexResponseParser {
    /// `origin_route` is the composed route's `Origin.route`; the model is the
    /// configured wire model, which `response.completed` may not echo.
    pub(crate) fn new(origin_route: &str, model: &str) -> Self {
        Self {
            route: origin_route.to_string(),
            model: model.to_string(),
            blocks: Vec::new(),
            terminal: None,
            response_id: None,
            call_names: std::collections::HashMap::new(),
            reasoning_emitted_in_item: false,
        }
    }

    /// Queue the single terminal outcome.
    fn finish(&mut self, outcome: Outcome) -> Vec<StreamEvent> {
        self.terminal = Some(outcome.clone());
        vec![StreamEvent::Finished(outcome)]
    }

    fn fail(&mut self, kind: ProviderErrorKind, message: &str) -> Vec<StreamEvent> {
        self.finish(Outcome::Failed(ProviderError::new(kind, message)))
    }

    /// Assemble the completed response from the accumulated blocks and the
    /// terminal envelope. `stop` is computed from the blocks for `completed` and
    /// supplied for `incomplete`.
    fn completed(&mut self, response: Option<&Value>, stop: StopReason) -> Vec<StreamEvent> {
        // Origin is ALWAYS the configured model, never the (dated alias) name the
        // response echoes: replay is gated on origin equality (providers.md ruling).
        let model = self.model.clone();
        let usage = response.and_then(parse_usage);
        let stop = match stop {
            StopReason::EndTurn if self.blocks.iter().any(is_tool_call) => StopReason::ToolUse,
            other => other,
        };
        let item = AssistantItem {
            origin: Origin {
                route: self.route.to_string(),
                model,
            },
            blocks: std::mem::take(&mut self.blocks),
        };
        self.finish(Outcome::Completed(CompletedResponse { item, stop, usage }))
    }

    /// Append one block for a completed output item. Unknown item types are
    /// ignored; the item is the completion unit for a call.
    fn handle_item_done(&mut self, item: &Value) {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let mut text = String::new();
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(part_text) = part.get("text").and_then(Value::as_str)
                        {
                            text.push_str(part_text);
                        }
                    }
                }
                self.blocks.push(AssistantBlock::Text { text });
            }
            Some("reasoning") => {
                let text = reasoning_summary_text(item);
                let replay = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|encrypted| !encrypted.is_empty())
                    .map(|encrypted| ReplayData {
                        origin: Origin {
                            route: self.route.to_string(),
                            model: self.model.clone(),
                        },
                        version: 1,
                        payload: serde_json::json!({
                            "type": "reasoning",
                            "encrypted_content": encrypted,
                        }),
                    });
                self.blocks.push(AssistantBlock::Reasoning { text, replay });
            }
            Some("function_call") => {
                let call_id = item_call_id(item);
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // RAW: never parsed, never repaired. A missing argument is "{}".
                let arguments = match item.get("arguments") {
                    Some(Value::String(raw)) => raw.clone(),
                    Some(Value::Null) | None => "{}".to_string(),
                    Some(other) => {
                        serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string())
                    }
                };
                self.blocks.push(AssistantBlock::ToolCall(ToolCall {
                    call_id,
                    name,
                    input: ToolInput::Json(arguments),
                }));
            }
            Some("custom_tool_call") => {
                let call_id = item_call_id(item);
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input = item
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.blocks.push(AssistantBlock::ToolCall(ToolCall {
                    call_id,
                    name,
                    input: ToolInput::Text(input),
                }));
            }
            _ => {}
        }
        self.reasoning_emitted_in_item = false;
    }
}

fn is_tool_call(block: &AssistantBlock) -> bool {
    matches!(block, AssistantBlock::ToolCall(_))
}

fn item_call_id(item: &Value) -> String {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn reasoning_summary_text(item: &Value) -> String {
    item.get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// Usage mapping from the terminal envelope. Absent fields stay `None`; a
/// missing usage object yields `None` rather than zeros. `input_tokens` is the
/// TOTAL input, so the cached and cache-written parts are subtracted out of it
/// to get `input_uncached`: an absent part counts as 0 in that subtraction
/// only, never in the reported value, and the subtraction saturates so that
/// uncached + read + write still adds up to the vendor's `input_tokens`.
fn parse_usage(response: &Value) -> Option<Usage> {
    let usage = response.get("usage")?;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
    let details = usage.get("input_tokens_details");
    let cached_tokens = details
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64);
    let cache_write_tokens = details
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(Value::as_u64);
    let output = usage.get("output_tokens").and_then(Value::as_u64);
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    let input_uncached = input_tokens.map(|input| {
        input
            .saturating_sub(cached_tokens.unwrap_or(0))
            .saturating_sub(cache_write_tokens.unwrap_or(0))
    });
    Some(Usage {
        input_uncached,
        cache_read: cached_tokens,
        cache_write: cache_write_tokens,
        output,
        reasoning_output: reasoning,
        cost_micro_usd: None,
    })
}

/// Map a provider error code/type to a contract kind. `code` is the raw wire
/// value; the returned message uses the sanitized form.
fn classify_error_code(code: Option<&str>) -> ProviderErrorKind {
    match code {
        Some("rate_limit_exceeded") | Some("usage_limit_reached") => ProviderErrorKind::RateLimited,
        Some("context_length_exceeded") => ProviderErrorKind::ContextWindowExceeded,
        Some("invalid_prompt") | Some("invalid_request_error") => ProviderErrorKind::InvalidRequest,
        Some(code) if is_auth_code(code) => ProviderErrorKind::Authentication,
        _ => ProviderErrorKind::Transport,
    }
}

fn is_auth_code(code: &str) -> bool {
    let code = code.to_ascii_lowercase();
    code.contains("auth")
        || code.contains("unauthorized")
        || code.contains("invalid_api_key")
        || code.contains("expired")
}

/// The error code/type from a stream error event. For `response.failed` the
/// error lives under `response.error`; for `error` it is top level.
fn stream_error_code(value: &Value, from_failed: bool) -> Option<&str> {
    let error = if from_failed {
        value
            .get("response")
            .and_then(|response| response.get("error"))
            .or_else(|| value.get("error"))
    } else {
        value.get("error")
    };
    if let Some(error) = error {
        if let Some(text) = error.as_str() {
            return Some(text);
        }
        if let Some(code) = error.get("code").and_then(Value::as_str) {
            return Some(code);
        }
        if let Some(kind) = error.get("type").and_then(Value::as_str) {
            return Some(kind);
        }
    }
    value.get("code").and_then(Value::as_str)
}

fn request_id(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
        .and_then(|(_, value)| safe_code(value).map(str::to_string))
}

impl ResponseParser for CodexResponseParser {
    fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent> {
        if self.terminal.is_some() {
            return Vec::new();
        }
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(_) => return self.fail(ProviderErrorKind::Protocol, "provider event is not JSON"),
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response.created") => {
                self.response_id = value
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Vec::new()
            }
            Some("response.output_text.delta") => match delta_text(&value) {
                Some(text) => vec![StreamEvent::TextDelta {
                    block: self.blocks.len(),
                    text,
                }],
                None => Vec::new(),
            },
            Some("response.reasoning_summary_text.delta") => {
                let events = match delta_text(&value) {
                    Some(text) => vec![StreamEvent::ReasoningDelta {
                        block: self.blocks.len(),
                        text,
                    }],
                    None => Vec::new(),
                };
                if !events.is_empty() {
                    self.reasoning_emitted_in_item = true;
                }
                events
            }
            Some("response.reasoning_summary_part.added") => {
                if self.reasoning_emitted_in_item {
                    vec![StreamEvent::ReasoningDelta {
                        block: self.blocks.len(),
                        text: "\n\n".to_string(),
                    }]
                } else {
                    Vec::new()
                }
            }
            Some("response.custom_tool_call_input.delta")
            | Some("response.function_call_arguments.delta") => {
                let Some(delta) = value.get("delta").and_then(Value::as_str) else {
                    return Vec::new();
                };
                if delta.is_empty() {
                    return Vec::new();
                }
                let call_id = value
                    .get("item_id")
                    .or_else(|| value.get("call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = self.call_names.get(&call_id).cloned().unwrap_or_default();
                vec![StreamEvent::ToolInputDelta {
                    call_id,
                    name,
                    text: delta.to_string(),
                }]
            }
            Some("response.output_item.added") => {
                if let Some(item) = value.get("item")
                    && matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call" | "custom_tool_call")
                    )
                    && let (Some(id), Some(name)) = (
                        item.get("id").and_then(Value::as_str),
                        item.get("name").and_then(Value::as_str),
                    )
                {
                    self.call_names.insert(id.to_string(), name.to_string());
                    if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                        self.call_names
                            .insert(call_id.to_string(), name.to_string());
                    }
                }
                Vec::new()
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item") {
                    self.handle_item_done(item);
                }
                Vec::new()
            }
            Some("response.completed") | Some("response.done") => {
                let response = value.get("response");
                self.completed(response, StopReason::EndTurn)
            }
            Some("response.incomplete") => {
                let response = value.get("response");
                let reason = response
                    .and_then(|response| response.get("incomplete_details"))
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str);
                let stop = match reason {
                    Some("max_output_tokens") => StopReason::MaxOutputTokens,
                    Some(reason) if reason.contains("context") => StopReason::ContextWindowExceeded,
                    _ => StopReason::Other,
                };
                self.completed(response, stop)
            }
            Some("response.failed") => {
                let code = stream_error_code(&value, true);
                let kind = classify_error_code(code);
                let label = code.and_then(safe_code).unwrap_or("unknown");
                self.fail(kind, &format!("provider error: {label}"))
            }
            Some("error") => {
                let code = stream_error_code(&value, false);
                let kind = classify_error_code(code);
                let label = code.and_then(safe_code).unwrap_or("unknown");
                self.fail(kind, &format!("provider error: {label}"))
            }
            _ => Vec::new(),
        }
    }

    fn on_end(&mut self) -> Outcome {
        self.terminal.clone().unwrap_or_else(|| {
            Outcome::Failed(ProviderError::new(
                ProviderErrorKind::Transport,
                "stream ended without a terminal event",
            ))
        })
    }

    fn on_http_error(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> ProviderError {
        let raw_code = http_error_code(body);
        let kind = if matches!(status, 400 | 413 | 422)
            && raw_code.as_deref() == Some("context_length_exceeded")
        {
            ProviderErrorKind::ContextWindowExceeded
        } else {
            kind_for_status(status).unwrap_or(ProviderErrorKind::InvalidRequest)
        };
        let mut message = format!("http {status}");
        if let Some(code) = raw_code.as_deref() {
            message.push(' ');
            message.push_str(code);
        }
        if let Some(request_id) = request_id(headers) {
            message.push_str(" x-request-id: ");
            message.push_str(&request_id);
        }
        ProviderError::new(kind, message)
    }
}

fn delta_text(value: &Value) -> Option<String> {
    value
        .get("delta")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use p1_contracts::{ProviderErrorKind, StopReason, StreamEvent};

    use super::*;

    fn event(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    fn feed(parser: &mut CodexResponseParser, data: &str) -> Vec<StreamEvent> {
        parser.on_event(event(data))
    }

    fn terminal(events: &[StreamEvent]) -> &Outcome {
        match events.last() {
            Some(StreamEvent::Finished(outcome)) => outcome,
            other => panic!("expected a terminal event, got {other:?}"),
        }
    }

    fn completed(events: &[StreamEvent]) -> &CompletedResponse {
        match terminal(events) {
            Outcome::Completed(response) => response,
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn output_text_deltas_use_the_item_block_index() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        assert_eq!(
            feed(
                &mut parser,
                r#"{"type":"response.output_text.delta","delta":"Hel"}"#
            ),
            vec![StreamEvent::TextDelta {
                block: 0,
                text: "Hel".to_string()
            }]
        );
        assert_eq!(
            feed(
                &mut parser,
                r#"{"type":"response.output_text.delta","delta":"lo"}"#
            ),
            vec![StreamEvent::TextDelta {
                block: 0,
                text: "lo".to_string()
            }]
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"Hello"}]}}"#,
        );
        assert!(events.is_empty(), "an item completion emits no delta");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
        );
        let response = completed(&events);
        assert_eq!(response.item.text(), "Hello");
        assert_eq!(response.stop, StopReason::EndTurn);
        assert!(response.usage.is_none());
    }

    #[test]
    fn message_text_concatenates_all_output_text_parts() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"Hello"},{"type":"output_text","text":" world"},{"type":"refusal","text":"no"}]}}"#,
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        assert_eq!(completed(&events).item.text(), "Hello world");
    }

    #[test]
    fn function_call_item_is_raw_and_marks_tool_use() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"src/main.rs\"}"}}"#,
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        let response = completed(&events);
        assert_eq!(response.stop, StopReason::ToolUse);
        let call = response.item.tool_calls().next().unwrap();
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, "read");
        assert_eq!(
            call.input,
            ToolInput::Json(r#"{"path":"src/main.rs"}"#.to_string())
        );
    }

    #[test]
    fn invalid_function_arguments_are_not_repaired() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\": "}}"#,
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        let call = completed(&events).item.tool_calls().next().unwrap().clone();
        assert_eq!(call.input, ToolInput::Json(r#"{"path": "#.to_string()));
    }

    #[test]
    fn custom_tool_call_item_carries_text_input() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch"}}"#,
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        let call = completed(&events).item.tool_calls().next().unwrap().clone();
        assert_eq!(call.input, ToolInput::Text("*** Begin Patch".to_string()));
    }

    #[test]
    fn blocks_keep_arrival_order() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"first"}]}}"#,
        );
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}}"#,
        );
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"second"}]}}"#,
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        let response = completed(&events);
        assert!(matches!(
            response.item.blocks[0],
            AssistantBlock::Text { .. }
        ));
        assert!(matches!(
            response.item.blocks[1],
            AssistantBlock::ToolCall(_)
        ));
        assert!(matches!(
            response.item.blocks[2],
            AssistantBlock::Text { .. }
        ));
        assert_eq!(response.item.text(), "first\nsecond");
    }

    #[test]
    fn reasoning_summary_deltas_and_section_break() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        assert!(
            feed(
                &mut parser,
                r#"{"type":"response.reasoning_summary_part.added","summary_index":0}"#
            )
            .is_empty()
        );
        assert_eq!(
            feed(
                &mut parser,
                r#"{"type":"response.reasoning_summary_text.delta","delta":"First "}"#
            ),
            vec![StreamEvent::ReasoningDelta {
                block: 0,
                text: "First ".to_string()
            }]
        );
        assert_eq!(
            feed(
                &mut parser,
                r#"{"type":"response.reasoning_summary_text.delta","delta":"thought."}"#
            ),
            vec![StreamEvent::ReasoningDelta {
                block: 0,
                text: "thought.".to_string()
            }]
        );
        assert_eq!(
            feed(
                &mut parser,
                r#"{"type":"response.reasoning_summary_part.added","summary_index":1}"#
            ),
            vec![StreamEvent::ReasoningDelta {
                block: 0,
                text: "\n\n".to_string()
            }],
            "a later part breaks the section only after visible reasoning"
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        assert!(completed(&events).item.blocks.is_empty());
    }

    #[test]
    fn custom_tool_input_and_function_argument_deltas_are_display_only() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        assert_eq!(
            feed(
                &mut parser,
                r#"{"type":"response.custom_tool_call_input.delta","item_id":"call_1","delta":"*** Begin"}"#
            ),
            vec![StreamEvent::ToolInputDelta {
                call_id: "call_1".to_string(),
                name: String::new(),
                text: "*** Begin".to_string()
            }]
        );
        assert_eq!(
            feed(
                &mut parser,
                r#"{"type":"response.function_call_arguments.delta","item_id":"call_2","delta":"{\"p"}"#
            ),
            vec![StreamEvent::ToolInputDelta {
                call_id: "call_2".to_string(),
                name: String::new(),
                text: "{\"p".to_string()
            }]
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        assert!(
            completed(&events).item.tool_calls().next().is_none(),
            "deltas never build calls"
        );
    }

    #[test]
    fn reasoning_item_replays_summary_and_encrypted_content() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc-1","summary":[{"type":"summary_text","text":"first"},{"type":"summary_text","text":"second"}]}}"#,
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        let response = completed(&events);
        match &response.item.blocks[0] {
            AssistantBlock::Reasoning { text, replay } => {
                assert_eq!(text, "first\n\nsecond");
                let replay = replay.as_ref().unwrap();
                assert_eq!(replay.origin.route, crate::ROUTE);
                assert_eq!(replay.origin.model, "gpt-test");
                assert_eq!(replay.version, 1);
                assert_eq!(replay.payload["encrypted_content"], "enc-1");
            }
            other => panic!("expected reasoning, got {other:?}"),
        }
    }

    #[test]
    fn encrypted_reasoning_without_summary_is_continuity_only() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        feed(
            &mut parser,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"enc-only"}}"#,
        );
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        match &completed(&events).item.blocks[0] {
            AssistantBlock::Reasoning { text, replay } => {
                assert_eq!(text, "");
                assert!(replay.is_some());
            }
            other => panic!("expected reasoning, got {other:?}"),
        }
    }

    #[test]
    fn usage_maps_distinct_fields_and_stays_none_when_absent() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":64,"cache_write_tokens":36},"output_tokens_details":{"reasoning_tokens":7}}}}"#,
        );
        let usage = completed(&events).usage.unwrap();
        assert_eq!(
            usage,
            Usage {
                input_uncached: Some(0),
                cache_read: Some(64),
                cache_write: Some(36),
                output: Some(20),
                reasoning_output: Some(7),
                cost_micro_usd: None,
            }
        );
        // The three input parts still add up to the vendor's TOTAL `input_tokens`.
        let parts =
            usage.input_uncached.unwrap() + usage.cache_read.unwrap() + usage.cache_write.unwrap();
        assert_eq!(parts, 100);

        // `cache_write_tokens` absent: `cache_write` stays `None`, not 0, and only
        // `cached_tokens` comes off the total.
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":64}}}}"#,
        );
        let usage = completed(&events).usage.unwrap();
        assert_eq!(usage.input_uncached, Some(36));
        assert_eq!(usage.cache_read, Some(64));
        assert_eq!(usage.cache_write, None);

        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":10}}}"#,
        );
        let usage = completed(&events).usage.unwrap();
        assert_eq!(usage.input_uncached, Some(10));
        assert_eq!(usage.cache_read, None);
        assert_eq!(usage.cache_write, None);
        assert_eq!(usage.output, None);

        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"id":"x"}}"#,
        );
        assert!(completed(&events).usage.is_none(), "absent usage is None");
    }

    #[test]
    fn input_uncached_saturates_when_cached_exceeds_input() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"input_tokens_details":{"cached_tokens":9}}}}"#,
        );
        assert_eq!(completed(&events).usage.unwrap().input_uncached, Some(0));

        // Cached plus cache-write exceeding the total also saturates at 0.
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":9}}}}"#,
        );
        let usage = completed(&events).usage.unwrap();
        assert_eq!(usage.input_uncached, Some(0));
        assert_eq!(usage.cache_write, Some(9));
    }

    #[test]
    fn completed_keeps_the_configured_model_as_origin() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "configured-model");
        let events = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{"model":"wire-model"}}"#,
        );
        assert_eq!(completed(&events).item.origin.model, "configured-model");
    }

    #[test]
    fn incomplete_reason_maps_to_stop_reason() {
        for (reason, expected) in [
            ("max_output_tokens", StopReason::MaxOutputTokens),
            ("context_length_exceeded", StopReason::ContextWindowExceeded),
            ("something_else", StopReason::Other),
        ] {
            let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
            let events = feed(
                &mut parser,
                &format!(
                    r#"{{"type":"response.incomplete","response":{{"incomplete_details":{{"reason":"{reason}"}}}}}}"#
                ),
            );
            assert_eq!(completed(&events).stop, expected);
        }
    }

    #[test]
    fn failed_and_error_events_map_the_code_and_hide_the_message() {
        let cases = [
            ("rate_limit_exceeded", ProviderErrorKind::RateLimited),
            ("usage_limit_reached", ProviderErrorKind::RateLimited),
            (
                "context_length_exceeded",
                ProviderErrorKind::ContextWindowExceeded,
            ),
            ("invalid_prompt", ProviderErrorKind::InvalidRequest),
            ("invalid_request_error", ProviderErrorKind::InvalidRequest),
            ("token_expired", ProviderErrorKind::Authentication),
            ("invalid_api_key", ProviderErrorKind::Authentication),
            ("weird_thing", ProviderErrorKind::Transport),
        ];
        for (code, kind) in cases {
            let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
            let events = feed(
                &mut parser,
                &format!(
                    r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}","message":"SENTINEL-BODY"}}}}}}"#
                ),
            );
            match terminal(&events) {
                Outcome::Failed(error) => {
                    assert_eq!(error.kind, kind, "code {code}");
                    assert!(error.message.contains(code), "{error:?}");
                    assert!(!error.message.contains("SENTINEL-BODY"), "{error:?}");
                }
                other => panic!("expected failure, got {other:?}"),
            }
        }

        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"SENTINEL-BODY"}}"#,
        );
        match terminal(&events) {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
                assert!(error.message.contains("invalid_request_error"));
                assert!(!error.message.contains("SENTINEL-BODY"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn hostile_error_codes_are_not_copied_verbatim() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(
            &mut parser,
            r#"{"type":"response.failed","response":{"error":{"code":"has spaces and sk-secret"}}}"#,
        );
        match terminal(&events) {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::Transport);
                assert!(error.message.contains("unknown"));
                assert!(!error.message.contains("sk-secret"));
                assert!(!error.message.contains("has spaces"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_a_protocol_failure() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let events = feed(&mut parser, "not json");
        assert!(matches!(
            terminal(&events),
            Outcome::Failed(error) if error.kind == ProviderErrorKind::Protocol
        ));
        assert!(parser.on_event(event("not json")).is_empty());
    }

    #[test]
    fn on_end_without_a_terminal_event_is_a_transport_failure() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        match parser.on_end() {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::Transport);
                assert_eq!(error.message, "stream ended without a terminal event");
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn done_and_empty_events_are_ignored() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        assert!(feed(&mut parser, "[DONE]").is_empty());
        assert!(parser.on_event(event("")).is_empty());
        assert!(parser.terminal.is_none());
    }

    #[test]
    fn nothing_is_emitted_after_the_terminal_event() {
        let mut parser = CodexResponseParser::new(crate::ROUTE, "gpt-test");
        let first = feed(
            &mut parser,
            r#"{"type":"response.completed","response":{}}"#,
        );
        assert_eq!(first.len(), 1);
        assert!(
            feed(
                &mut parser,
                r#"{"type":"response.output_text.delta","delta":"stray"}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn http_errors_classify_and_redact_the_body() {
        let headers = vec![
            ("x-request-id".to_string(), "req_123".to_string()),
            ("x-secret".to_string(), "SENTINEL-HEADER".to_string()),
        ];
        let context = parser_error(
            &headers,
            400,
            br#"{"error":{"code":"context_length_exceeded","message":"SENTINEL-BODY"}}"#,
        );
        assert_eq!(context.kind, ProviderErrorKind::ContextWindowExceeded);
        assert!(context.message.contains("x-request-id: req_123"));
        assert!(!context.message.contains("SENTINEL-BODY"));

        for (status, kind) in [
            (401, ProviderErrorKind::Authentication),
            (403, ProviderErrorKind::Authentication),
            (429, ProviderErrorKind::RateLimited),
            (408, ProviderErrorKind::Transport),
            (500, ProviderErrorKind::Transport),
            (503, ProviderErrorKind::Transport),
            (422, ProviderErrorKind::InvalidRequest),
            (400, ProviderErrorKind::InvalidRequest),
        ] {
            let body: &[u8] = if status == 400 {
                br#"{"error":{"code":"bad_request"}}"#
            } else {
                b""
            };
            assert_eq!(
                parser_error(&[], status, body).kind,
                kind,
                "status {status}"
            );
        }

        let with_code = parser_error(
            &headers,
            400,
            br#"{"error":{"type":"invalid_request_error"}}"#,
        );
        assert!(with_code.message.contains("400 invalid_request_error"));

        let no_body_text = parser_error(&headers, 400, br#"{"error":{"message":"SENTINEL-BODY"}}"#);
        assert!(!no_body_text.message.contains("SENTINEL-BODY"));
        assert!(!no_body_text.message.contains("SENTINEL-HEADER"));
    }

    fn parser_error(headers: &[(String, String)], status: u16, body: &[u8]) -> ProviderError {
        CodexResponseParser::new(crate::ROUTE, "gpt-test").on_http_error(status, headers, body)
    }
}
