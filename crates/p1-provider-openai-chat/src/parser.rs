use crate::ChatDialect;
use p1_contracts::{
    AssistantBlock, AssistantItem, CompletedResponse, Origin, Outcome, ProviderError,
    ProviderErrorKind, ReplayData, StopReason, StreamEvent, ToolCall, ToolInput, Usage,
};
use p1_provider_http::{ResponseParser, SseEvent, http_error_code, kind_for_status, reset_after};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) struct ChatParser {
    origin: Origin,
    blocks: Vec<AssistantBlock>,
    calls: BTreeMap<u64, usize>,
    text: Option<usize>,
    reasoning: Option<usize>,
    stop: Option<StopReason>,
    usage: Option<Usage>,
    ended: bool,
}
impl ChatParser {
    pub(crate) fn new(origin: Origin, _dialect: ChatDialect) -> Self {
        Self {
            origin,
            blocks: Vec::new(),
            calls: BTreeMap::new(),
            text: None,
            reasoning: None,
            stop: None,
            usage: None,
            ended: false,
        }
    }
    fn delta(&mut self, text: &str, reasoning: bool) -> StreamEvent {
        if reasoning {
            self.text = None;
        } else {
            self.reasoning = None;
        }
        let slot = if reasoning {
            &mut self.reasoning
        } else {
            &mut self.text
        };
        let index = *slot.get_or_insert_with(|| {
            let index = self.blocks.len();
            self.blocks.push(if reasoning {
                AssistantBlock::Reasoning {
                    text: String::new(),
                    replay: None,
                }
            } else {
                AssistantBlock::Text {
                    text: String::new(),
                }
            });
            index
        });
        match &mut self.blocks[index] {
            AssistantBlock::Text { text: all } | AssistantBlock::Reasoning { text: all, .. } => {
                all.push_str(text)
            }
            _ => unreachable!(),
        }
        if reasoning {
            StreamEvent::ReasoningDelta {
                block: index,
                text: text.into(),
            }
        } else {
            StreamEvent::TextDelta {
                block: index,
                text: text.into(),
            }
        }
    }
    fn fail(&mut self, message: &str) -> Vec<StreamEvent> {
        self.ended = true;
        vec![StreamEvent::Finished(Outcome::Failed(ProviderError::new(
            ProviderErrorKind::Protocol,
            message,
        )))]
    }
    fn complete(&mut self) -> Vec<StreamEvent> {
        let Some(stop) = self.stop else {
            return self.fail("chat stream has no finish reason");
        };
        if stop == StopReason::ToolUse {
            if self.calls.is_empty() {
                return self.fail("tool finish without calls");
            }
            let mut ids = std::collections::BTreeSet::new();
            for block in &self.blocks {
                if let AssistantBlock::ToolCall(call) = block
                    && (call.call_id.is_empty()
                        || call.name.is_empty()
                        || !ids.insert(&call.call_id))
                {
                    return self.fail("incomplete or duplicate tool identity");
                }
            }
        } else if !self.calls.is_empty() {
            if stop == StopReason::MaxOutputTokens {
                // A token-limited call is not executable even if its arguments happen to parse.
                self.blocks
                    .retain(|b| !matches!(b, AssistantBlock::ToolCall(_)));
            } else {
                return self.fail("tool calls without a tool finish reason");
            }
        }
        for block in &mut self.blocks {
            if let AssistantBlock::Reasoning { text, replay } = block {
                *replay = Some(ReplayData {
                    origin: self.origin.clone(),
                    version: 1,
                    payload: Value::String(text.clone()),
                });
            }
        }
        self.ended = true;
        vec![StreamEvent::Finished(Outcome::Completed(
            CompletedResponse {
                item: AssistantItem {
                    origin: self.origin.clone(),
                    blocks: std::mem::take(&mut self.blocks),
                },
                stop,
                usage: self.usage,
            },
        ))]
    }
}
impl ResponseParser for ChatParser {
    fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        if event.data.trim() == "[DONE]" {
            return self.complete();
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            return self.fail("invalid chat stream JSON");
        };
        if value.get("error").is_some() || event.event.as_deref() == Some("error") {
            return self.fail("chat provider error event");
        }
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(map_usage(usage));
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return self.fail("chat chunk missing choices");
        };
        let mut events = Vec::new();
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64) != Some(0) {
                return self.fail("unexpected chat choice index");
            }
            if self.stop.is_some() {
                // OpenCode Zen's free tier repeats the terminal `finish_reason` choice
                // beside the usage block (instead of `choices: []`). It carries no model
                // output, so it is tolerated; a choice with content after the stop is
                // still a protocol error.
                if is_empty_terminator(choice) {
                    continue;
                }
                return self.fail("choice after finish reason");
            }
            let Some(delta) = choice.get("delta").filter(|v| v.is_object()) else {
                return self.fail("chat choice missing delta");
            };
            for field in ["content", "reasoning_content", "reasoning"] {
                if delta
                    .get(field)
                    .is_some_and(|value| !value.is_null() && !value.is_string())
                {
                    return self.fail("invalid chat text delta type");
                }
            }
            if delta
                .get("tool_calls")
                .is_some_and(|value| !value.is_null() && !value.is_array())
            {
                return self.fail("invalid tool delta list");
            }
            // Both dialects declare the alias equivalent to the replayable reasoning field.
            // When both arrive in one delta, reasoning_content wins and the alias is ignored.
            if let Some(part) = delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .or_else(|| delta.get("reasoning").and_then(Value::as_str))
                && !part.is_empty()
            {
                events.push(self.delta(part, true));
            }
            if let Some(part) = delta.get("content").and_then(Value::as_str)
                && !part.is_empty()
            {
                events.push(self.delta(part, false));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.text = None;
                    self.reasoning = None;
                    let Some(index) = call.get("index").and_then(Value::as_u64) else {
                        return self.fail("tool delta missing index");
                    };
                    // Continuation chunks of a streamed call may repeat the field as
                    // `null` or `""` (seen on the opencode-go route: two long runs died
                    // here). Only a NAMED other type is unsupported.
                    if call
                        .get("type")
                        .is_some_and(|value| !value.is_null() && value != "" && value != "function")
                    {
                        // Name the type: a short, sanitised word, never stream content.
                        let named: String = call["type"]
                            .as_str()
                            .unwrap_or("<non-string>")
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                            .take(32)
                            .collect();
                        return self.fail(&format!("unsupported chat tool type `{named}`"));
                    }
                    for field in ["/id", "/function/name", "/function/arguments"] {
                        if call
                            .pointer(field)
                            .is_some_and(|value| !value.is_null() && !value.is_string())
                        {
                            return self.fail("invalid tool fragment type");
                        }
                    }
                    let block = *self.calls.entry(index).or_insert_with(|| {
                        let block = self.blocks.len();
                        self.blocks.push(AssistantBlock::ToolCall(ToolCall {
                            call_id: String::new(),
                            name: String::new(),
                            input: ToolInput::Json(String::new()),
                        }));
                        block
                    });
                    let AssistantBlock::ToolCall(buffer) = &mut self.blocks[block] else {
                        unreachable!()
                    };
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        buffer.call_id.push_str(id);
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        buffer.name.push_str(name);
                    }
                    if let Some(part) = call.pointer("/function/arguments").and_then(Value::as_str)
                    {
                        let ToolInput::Json(raw) = &mut buffer.input else {
                            unreachable!()
                        };
                        raw.push_str(part);
                        events.push(StreamEvent::ToolInputDelta {
                            call_id: buffer.call_id.clone(),
                            name: buffer.name.clone(),
                            text: part.into(),
                        });
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.stop = Some(match reason {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" => StopReason::ToolUse,
                    "length" => StopReason::MaxOutputTokens,
                    "content_filter" => StopReason::Refusal,
                    _ => StopReason::Other,
                });
            }
        }
        events
    }
    fn on_end(&mut self) -> Outcome {
        self.ended = true;
        Outcome::Failed(ProviderError::new(
            ProviderErrorKind::Transport,
            "chat stream ended before [DONE]",
        ))
    }
    fn on_http_error(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> ProviderError {
        // A usage-limit error is an exhausted allowance, not a short rate-limit
        // window. Retrying it across the provider's reset only hides the failure.
        if matches!(status, 402 | 429) && names_usage_limit(body) {
            let message = match reset_after(headers) {
                Some(delay) => format!(
                    "{USAGE_LIMIT_MESSAGE} (resets in {})",
                    human_duration(delay)
                ),
                None => USAGE_LIMIT_MESSAGE.to_string(),
            };
            return ProviderError::new(ProviderErrorKind::UsageLimitExhausted, message);
        }
        // A 401/403 whose body says the account has no balance is not a rejected
        // key (ADR-0046): refreshing the credential cannot help, so the operator
        // must read the balance, not a key error. A plan / free-tier refusal is
        // the same shape with a different diagnosis (ADR-0062): the key is valid,
        // the route is just not entitled to this model, so a refresh cannot help
        // either.
        if matches!(status, 401 | 403) {
            if names_no_balance(body) {
                return ProviderError::new(
                    ProviderErrorKind::InsufficientBalance,
                    NO_BALANCE_MESSAGE,
                );
            }
            if names_plan_refusal(body) {
                return ProviderError::new(ProviderErrorKind::NotEntitled, NOT_ENTITLED_MESSAGE);
            }
        }
        let context = serde_json::from_slice::<Value>(body).ok().is_some_and(|v| {
            v.pointer("/error/code").and_then(Value::as_str) == Some("context_length_exceeded")
        });
        let kind = if context {
            ProviderErrorKind::ContextWindowExceeded
        } else {
            kind_for_status(status).unwrap_or(ProviderErrorKind::InvalidRequest)
        };
        // The operator needs to know WHAT the endpoint refused (a 400 without a reason
        // cannot be acted on). Only a short, token-shaped code is copied — never the
        // server's free text — as the two sibling adapters already do.
        match http_error_code(body) {
            Some(code) => ProviderError::new(kind, format!("chat HTTP status {status} ({code})")),
            None => ProviderError::new(kind, format!("chat HTTP status {status}")),
        }
    }
}

/// Whether a post-stop choice is an empty terminator: a repeated `finish_reason`
/// with no content, reasoning or tool fragment in its delta. Some gateways send
/// one together with the usage block; it must not become model output or an
/// error.
fn is_empty_terminator(choice: &Value) -> bool {
    if choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .is_none()
    {
        return false;
    }
    let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
        return false;
    };
    ["content", "reasoning_content", "reasoning", "tool_calls"]
        .iter()
        .all(|field| match delta.get(*field) {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.is_empty(),
            Some(Value::Array(items)) => items.is_empty(),
            Some(_) => false,
        })
}

/// The whole text of an exhausted usage allowance: only the provider's reset
/// hint is formatted into it, never the server's free text.
const USAGE_LIMIT_MESSAGE: &str = "the account's usage allowance is used up";

/// Fixed usage-limit/quota words, read in the same four JSON positions as the
/// no-balance and plan-refusal classifications.
const USAGE_LIMIT_WORDS: [&str; 3] = [
    "gousagelimiterror",
    "insufficient_quota",
    "usage_limit_exceeded",
];

fn names_usage_limit(body: &[u8]) -> bool {
    names_any_position(body, &USAGE_LIMIT_WORDS)
}

fn human_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    let parts = [
        (total_seconds / 86_400, "d"),
        ((total_seconds % 86_400) / 3_600, "h"),
        ((total_seconds % 3_600) / 60, "min"),
        (total_seconds % 60, "s"),
    ];
    let mut out = String::new();
    for (amount, unit) in parts {
        if amount > 0 {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&amount.to_string());
            out.push(' ');
            out.push_str(unit);
        }
    }
    if out.is_empty() {
        "0 s".to_string()
    } else {
        out
    }
}

/// The whole text of an exhausted-account error: the server's words are a lookup
/// key only, never copied, sliced or formatted into the message (ADR-0046).
const NO_BALANCE_MESSAGE: &str = "the account has no balance";

/// The fixed allow-list of error words that mean "this account has no balance".
/// A guess about wire shapes nobody could call live: an unknown shape falls back
/// to the status-based classification, which is safe and merely unhelpful.
const NO_BALANCE_WORDS: [&str; 5] = [
    "creditserror",
    "insufficient_balance",
    "insufficient_quota",
    "quota_exceeded",
    "billing_error",
];

/// The whole text of a plan / free-tier refusal (ADR-0062): like the no-balance
/// message it is a constant, never the server's free text.
const NOT_ENTITLED_MESSAGE: &str = "the account's plan does not allow this model on this route";

/// The fixed allow-list of error words that mean "the plan does not allow this".
/// `freetiererror` is the observed OpenCode Zen shape (a valid key, a model gated
/// to OpenCode's own client); the rest are the same fixed-guess kind the no-balance
/// list is, an unknown shape falling back to the status-based classification.
const NOT_ENTITLED_WORDS: [&str; 3] = ["freetiererror", "not_entitled", "plan_not_allowed"];

/// Whether an error body names a no-balance word in one of the four fixed
/// positions. The body is classification input: a non-JSON, empty or differently
/// shaped body is simply not a hit.
fn names_no_balance(body: &[u8]) -> bool {
    names_any_position(body, &NO_BALANCE_WORDS)
}

/// Whether an error body names a plan-refusal word in one of the same four fixed
/// positions `names_no_balance` reads (ADR-0062).
fn names_plan_refusal(body: &[u8]) -> bool {
    names_any_position(body, &NOT_ENTITLED_WORDS)
}

/// Read `/error/type`, `/error/code`, top-level `/type` and `/code` (strings
/// only), lower-case them and look each up in `words`.
fn names_any_position(body: &[u8], words: &[&str]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    ["/error/type", "/error/code", "/type", "/code"]
        .iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .any(|word| words.contains(&word.to_ascii_lowercase().as_str()))
}

fn map_usage(value: &Value) -> Usage {
    let cache_read = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| value.get("prompt_cache_hit_tokens").and_then(Value::as_u64));
    let total = value.get("prompt_tokens").and_then(Value::as_u64);
    Usage {
        input_uncached: value
            .get("prompt_cache_miss_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                total.and_then(|total| cache_read.and_then(|cached| total.checked_sub(cached)))
            }),
        cache_read,
        output: value.get("completion_tokens").and_then(Value::as_u64),
        reasoning_output: value
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        ..Usage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatDialect;
    use serde_json::json;
    fn parser() -> ChatParser {
        ChatParser::new(
            crate::test_config::route(false).origin("configured"),
            ChatDialect::ThinkingWithReasoningAlias,
        )
    }

    #[test]
    fn rejected_http_error_code_is_not_displayed() {
        let error = parser().on_http_error(400, &[], br#"{"error":{"code":"has spaces"}}"#);
        assert_eq!(error.message, "chat HTTP status 400");
        assert!(!error.message.contains("has spaces"));
    }

    #[test]
    fn an_unrecognised_429_body_stays_rate_limited() {
        let error =
            parser().on_http_error(429, &[], br#"{"error":{"type":"temporary_burst_limit"}}"#);
        assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    }

    #[test]
    fn a_free_tier_refusal_is_not_entitled_not_authentication() {
        // The live OpenCode Zen shape: a valid key, but the model is gated to
        // OpenCode's own client (ADR-0062).
        let error = parser().on_http_error(
            403,
            &[],
            br#"{"error":{"type":"FreeTierError","message":"this model is only available in the OpenCode client"}}"#,
        );
        assert_eq!(error.kind, ProviderErrorKind::NotEntitled);
        assert_eq!(error.message, NOT_ENTITLED_MESSAGE);
        // The server's free text is never copied into the message.
        assert!(
            !error
                .message
                .contains("only available in the OpenCode client")
        );

        // An unknown 403 body keeps today's status-based classification.
        let unknown = parser().on_http_error(403, &[], br#"{"error":{"type":"invalid_api_key"}}"#);
        assert_eq!(unknown.kind, ProviderErrorKind::Authentication);

        // The no-balance classification is unchanged.
        let balance = parser().on_http_error(401, &[], br#"{"error":{"type":"creditserror"}}"#);
        assert_eq!(balance.kind, ProviderErrorKind::InsufficientBalance);
        assert_eq!(balance.message, NO_BALANCE_MESSAGE);
    }

    fn send(p: &mut ChatParser, value: Value) -> Vec<StreamEvent> {
        p.on_event(SseEvent {
            event: None,
            data: value.to_string(),
        })
    }
    fn choice(delta: Value, finish: Value) -> Value {
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }
    fn done(p: &mut ChatParser) -> CompletedResponse {
        match p
            .on_event(SseEvent {
                event: None,
                data: "[DONE]".into(),
            })
            .pop()
            .unwrap()
        {
            StreamEvent::Finished(Outcome::Completed(response)) => response,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_usage_limit_is_terminal_and_its_reset_hint_is_sanitised() {
        for (header, value) in [("Retry-After", "187200"), ("X-RateLimit-Reset", "187200")] {
            let error = parser().on_http_error(
                429,
                &[(header.into(), value.into())],
                br#"{"error":{"type":"GoUsageLimitError","message":"wire text"}}"#,
            );
            assert_eq!(error.kind, ProviderErrorKind::UsageLimitExhausted);
            assert_eq!(
                error.message,
                "the account's usage allowance is used up (resets in 2 d 4 h)"
            );
            assert!(!error.message.contains("wire text"));
        }
    }

    #[test]
    fn a_null_or_empty_tool_type_on_a_continuation_chunk_is_not_a_protocol_error() {
        for filler in [json!(null), json!("")] {
            let mut p = parser();
            send(
                &mut p,
                choice(
                    json!({"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":"{\"file_"}}]}),
                    json!(null),
                ),
            );
            let events = send(
                &mut p,
                choice(
                    json!({"tool_calls":[{"index":0,"type":filler,"function":{"arguments":"path\":\"a\"}"}}]}),
                    json!("tool_calls"),
                ),
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::Finished(Outcome::Failed(_)))),
                "{events:?}"
            );
            let response = done(&mut p);
            assert_eq!(response.stop, p1_contracts::StopReason::ToolUse);
        }
    }
    #[test]
    fn a_named_other_tool_type_is_still_a_protocol_error() {
        let mut p = parser();
        let events = send(
            &mut p,
            choice(
                json!({"tool_calls":[{"index":0,"id":"c1","type":"custom","function":{"name":"read","arguments":"{}"}}]}),
                json!(null),
            ),
        );
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Finished(Outcome::Failed(ProviderError {
                kind: ProviderErrorKind::Protocol,
                ..
            })))
        ));
    }
    #[test]
    fn usage_does_not_invent_cache_or_uncached_counts() {
        let usage = map_usage(&json!({"prompt_tokens":100,"completion_tokens":20}));
        assert_eq!(usage.input_uncached, None);
        assert_eq!(usage.cache_read, None);
        assert_eq!(usage.output, Some(20));
        assert_eq!(usage.cost_micro_usd, None);
        let usage = map_usage(
            &json!({"prompt_tokens":100,"prompt_cache_hit_tokens":70,"prompt_cache_miss_tokens":30}),
        );
        assert_eq!(usage.input_uncached, Some(30));
        assert_eq!(usage.cache_read, Some(70));
        assert_eq!(
            map_usage(&json!({"prompt_tokens":1,"prompt_tokens_details":{"cached_tokens":2}}))
                .input_uncached,
            None
        );
    }
    #[test]
    fn usage_after_finish_is_retained_but_eof_is_not_completion() {
        let mut p = parser();
        send(&mut p, choice(json!({"content":"hello"}), json!("stop")));
        send(
            &mut p,
            json!({"choices":[],"usage":{"completion_tokens":9}}),
        );
        assert_eq!(done(&mut p).usage.unwrap().output, Some(9));
        let mut p = parser();
        send(&mut p, choice(json!({}), json!("stop")));
        assert!(matches!(
            p.on_end(),
            Outcome::Failed(ProviderError {
                kind: ProviderErrorKind::Transport,
                ..
            })
        ));
    }
    #[test]
    fn a_repeated_empty_terminator_with_usage_completes_instead_of_failing() {
        // `mimo-v2.6-flash-free` on the OpenCode Zen free tier sends: a content
        // chunk, the stop chunk, a SECOND stop chunk carrying the usage block,
        // then [DONE]. The repeat is not model output and must not be an error.
        let mut p = parser();
        send(&mut p, choice(json!({"content":"OK"}), json!(null)));
        send(
            &mut p,
            choice(
                json!({"role":"assistant","content":"","reasoning":null}),
                json!("stop"),
            ),
        );
        let events = send(
            &mut p,
            json!({
                "choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":131,"completion_tokens":38,
                         "prompt_tokens_details":{"cached_tokens":0},
                         "completion_tokens_details":{"reasoning_tokens":35}}
            }),
        );
        assert!(events.is_empty(), "{events:?}");
        let response = done(&mut p);
        assert_eq!(response.stop, StopReason::EndTurn);
        assert_eq!(
            response.item.blocks,
            [AssistantBlock::Text { text: "OK".into() }]
        );
        let usage = response.usage.unwrap();
        assert_eq!(usage.output, Some(38));
        assert_eq!(usage.reasoning_output, Some(35));
        // A genuine content choice after the stop is still refused.
        let mut p = parser();
        send(&mut p, choice(json!({}), json!("stop")));
        let failed = send(&mut p, choice(json!({"content":"stray"}), json!(null)));
        assert!(
            matches!(
                failed.as_slice(),
                [StreamEvent::Finished(Outcome::Failed(_))]
            ),
            "{failed:?}"
        );
    }

    #[test]
    fn interleaved_call_fragments_preserve_order_and_raw_arguments() {
        let mut p = parser();
        send(
            &mut p,
            choice(
                json!({"tool_calls":[
                    {"index":0,"id":"a","function":{"name":"read","arguments":"{"}},
                    {"index":1,"id":"b","function":{"name":"grep","arguments":"{"}}
                ]}),
                Value::Null,
            ),
        );
        send(
            &mut p,
            choice(
                json!({"tool_calls":[
                    {"index":1,"function":{"arguments":"\"pattern\":\"雪\"}"}},
                    {"index":0,"function":{"arguments":"\"path\":\"a.txt\"}"}}
                ]}),
                json!("tool_calls"),
            ),
        );
        let response = done(&mut p);
        let calls: Vec<_> = response.item.tool_calls().collect();
        assert_eq!(calls[0].call_id, "a");
        assert_eq!(calls[1].call_id, "b");
        assert_eq!(calls[1].input.raw(), "{\"pattern\":\"雪\"}");
    }
    #[test]
    fn token_limit_does_not_execute_partial_calls() {
        let mut p = parser();
        send(
            &mut p,
            choice(
                json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"write","arguments":"{"}}]}),
                json!("length"),
            ),
        );
        let response = done(&mut p);
        assert_eq!(response.stop, StopReason::MaxOutputTokens);
        assert_eq!(response.item.tool_calls().count(), 0);
    }
    #[test]
    fn missing_identity_and_duplicate_ids_are_protocol_failures() {
        for calls in [
            json!([{"index":0,"function":{"name":"read","arguments":"{}"}}]),
            json!([
                {"index":0,"id":"a","function":{"name":"read","arguments":"{}"}},
                {"index":1,"id":"a","function":{"name":"write","arguments":"{}"}}
            ]),
        ] {
            let mut p = parser();
            send(
                &mut p,
                choice(json!({"tool_calls":calls}), json!("tool_calls")),
            );
            assert!(matches!(
                p.on_event(SseEvent {
                    event: None,
                    data: "[DONE]".into()
                })
                .last(),
                Some(StreamEvent::Finished(Outcome::Failed(_)))
            ));
        }
    }
    #[test]
    fn go_reasoning_alias_replays_with_configured_origin() {
        let mut p = parser();
        send(
            &mut p,
            json!({"model":"server-dated-alias","choices":[{"index":0,"delta":{"reasoning":"preserve 雪\n"},"finish_reason":null}]}),
        );
        send(&mut p, choice(json!({"content":"answer"}), json!("stop")));
        let response = done(&mut p);
        let AssistantBlock::Reasoning {
            replay: Some(replay),
            ..
        } = &response.item.blocks[0]
        else {
            panic!()
        };
        assert_eq!(replay.origin.model, "configured");
        assert_eq!(replay.payload, json!("preserve 雪\n"));
    }

    #[test]
    fn retained_thinking_accepts_reasoning_alias_as_reasoning_delta() {
        let mut p = ChatParser::new(
            crate::test_config::route(true).origin("configured"),
            ChatDialect::RetainedThinking,
        );
        let events = send(
            &mut p,
            choice(json!({"reasoning":"preserve 雪\n"}), json!(null)),
        );
        assert_eq!(
            events,
            vec![StreamEvent::ReasoningDelta {
                block: 0,
                text: "preserve 雪\n".into(),
            }]
        );
    }

    #[test]
    fn reasoning_content_takes_precedence_over_reasoning_alias_in_both_dialects() {
        for dialect in [
            ChatDialect::ThinkingWithReasoningAlias,
            ChatDialect::RetainedThinking,
        ] {
            let mut p = ChatParser::new(
                crate::test_config::route(matches!(dialect, ChatDialect::RetainedThinking))
                    .origin("configured"),
                dialect,
            );
            let events = send(
                &mut p,
                choice(
                    json!({"reasoning_content":"canonical", "reasoning":"alias"}),
                    json!(null),
                ),
            );
            assert_eq!(
                events,
                vec![StreamEvent::ReasoningDelta {
                    block: 0,
                    text: "canonical".into(),
                }],
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn non_string_reasoning_alias_keeps_the_existing_type_error() {
        let mut p = ChatParser::new(
            crate::test_config::route(true).origin("configured"),
            ChatDialect::RetainedThinking,
        );
        let events = send(&mut p, choice(json!({"reasoning": 17}), json!(null)));
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Finished(Outcome::Failed(ProviderError {
                kind: ProviderErrorKind::Protocol,
                message,
            }))] if message == "invalid chat text delta type"
        ));
    }
}

#[cfg(test)]
mod block_order_tests {
    use super::*;
    use crate::ChatDialect;
    use serde_json::json;
    #[test]
    fn text_and_reasoning_blocks_keep_their_sequence() {
        let mut p = ChatParser::new(
            crate::test_config::route(true).origin("m"),
            ChatDialect::RetainedThinking,
        );
        for delta in [
            json!({"reasoning_content":"first"}),
            json!({"content":"middle"}),
            json!({"reasoning_content":"last"}),
        ] {
            p.on_event(SseEvent {
                event: None,
                data: json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]})
                    .to_string(),
            });
        }
        assert!(
            matches!(&p.blocks[..], [AssistantBlock::Reasoning{text:a,..},AssistantBlock::Text{text:b},AssistantBlock::Reasoning{text:c,..}] if a=="first" && b=="middle" && c=="last")
        );
    }
}

#[cfg(test)]
mod malformed_tests {
    use super::*;
    use crate::ChatDialect;
    use serde_json::json;
    #[test]
    fn malformed_deltas_fail_instead_of_becoming_empty_success() {
        for delta in [
            json!({"content":17}),
            json!({"tool_calls":{}}),
            json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":{}}}]}),
        ] {
            let mut parser = ChatParser::new(
                crate::test_config::route(true).origin("m"),
                ChatDialect::RetainedThinking,
            );
            let events = parser.on_event(SseEvent {
                event: None,
                data: json!({"choices":[{"index":0,"delta":delta,"finish_reason":"stop"}]})
                    .to_string(),
            });
            assert!(matches!(
                events.last(),
                Some(StreamEvent::Finished(Outcome::Failed(ProviderError {
                    kind: ProviderErrorKind::Protocol,
                    ..
                })))
            ));
        }
    }
}
