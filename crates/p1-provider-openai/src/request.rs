//! Pure wire translation for the Codex subscription route.
//!
//! Everything here is synchronous and side-effect free so it can be tested
//! against golden JSON without a transport. The authoritative wire facts are
//! `docs/design/routes.md` §B.

use std::collections::HashMap;

use p1_contracts::history::{AssistantBlock, Item, Origin, ReplayData, ToolInput};
use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{
    Effort, ModelOptions, ProviderError, ProviderErrorKind, ProviderRequest, serde_json,
};
use p1_provider_http::Credential;
use serde_json::{Map, Value, json};

/// Stable route identifier. Replay data is only valid for this route.
pub const ROUTE: &str = "openai-responses/codex-subscription";

/// Namespace an adapter owns inside `ModelOptions::native`.
const NATIVE_PREFIX: &str = "openai-responses.";
const VERBOSITY_KEY: &str = "openai-responses.verbosity";

/// Default endpoint base. `with_base_url` overrides it for tests and for a
/// proxy; the route path is appended by [`resolve_base_url`].
pub(crate) const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// Resolve the responses endpoint from a base URL. A trailing slash, an
/// already-complete path and a bare `/codex` base all work; anything that is not
/// `http(s)://` is rejected before a request is attempted.
pub fn resolve_base_url(base: &str) -> Result<String, ProviderError> {
    let base = base.trim();
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "invalid Codex base URL",
        ));
    }
    let base = base.trim_end_matches('/');
    Ok(if base.ends_with("/codex/responses") {
        base.to_string()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    })
}

/// Build the fixed header set for this route. When `cache_key` is set it is
/// also sent as `session_id` and `conversation_id`, so the wire's session
/// identity matches the body. The credential is the only other input; an absent
/// account id is an authentication failure because the route cannot name the
/// ChatGPT account without it.
pub fn build_headers(
    credential: &Credential,
    cache_key: Option<&str>,
) -> Result<Vec<(String, String)>, ProviderError> {
    let account_id = credential.account_id.as_deref().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Codex credential has no ChatGPT account id",
        )
    })?;
    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", credential.bearer),
        ),
        ("chatgpt-account-id".to_string(), account_id.to_string()),
        ("originator".to_string(), "p1".to_string()),
        (
            "User-Agent".to_string(),
            format!("p1/{}", env!("CARGO_PKG_VERSION")),
        ),
        (
            "OpenAI-Beta".to_string(),
            "responses=experimental".to_string(),
        ),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
    ];
    if let Some(key) = cache_key {
        headers.push(("session_id".to_string(), key.to_string()));
        headers.push(("conversation_id".to_string(), key.to_string()));
    }
    Ok(headers)
}

/// Reject what the route cannot carry before a run starts.
pub(crate) fn validate(options: &ModelOptions) -> Result<(), ProviderError> {
    if options.max_output_tokens.is_some() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "this route does not accept max_output_tokens",
        ));
    }
    if matches!(
        options.reasoning_effort,
        Some(Effort::ExtraHigh | Effort::Max)
    ) {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "this route does not accept reasoning_effort extra_high or max",
        ));
    }
    for key in options.native.keys() {
        if let Some(rest) = key.strip_prefix(NATIVE_PREFIX)
            && rest != "verbosity"
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("unknown openai-responses option: {key}"),
            ));
        }
    }
    verbosity(options)?;
    Ok(())
}

fn verbosity(options: &ModelOptions) -> Result<&'static str, ProviderError> {
    match options.native.get(VERBOSITY_KEY) {
        None => Ok("low"),
        Some(Value::String(value)) => match value.as_str() {
            "low" => Ok("low"),
            "medium" => Ok("medium"),
            "high" => Ok("high"),
            _ => Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "openai-responses.verbosity must be one of low, medium, high",
            )),
        },
        Some(_) => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "openai-responses.verbosity must be one of low, medium, high",
        )),
    }
}

fn reasoning_effort(effort: Effort) -> Result<&'static str, ProviderError> {
    match effort {
        Effort::Low => Ok("low"),
        Effort::Medium => Ok("medium"),
        Effort::High => Ok("high"),
        Effort::ExtraHigh | Effort::Max => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "this route does not accept reasoning_effort extra_high or max",
        )),
    }
}

/// The route's `prompt_cache_key` for a request: clamped to the 64-character
/// cap the wire enforces, or `None` when the caller asked for no caching. The
/// body and the session headers both go through here so they cannot diverge.
pub(crate) fn clamped_cache_key(options: &ModelOptions) -> Option<String> {
    options
        .cache_key
        .as_ref()
        .map(|key| key.chars().take(64).collect())
}

/// Build the request body. Pure: no transport, no clock, no credentials.
pub fn build_request(model: &str, request: &ProviderRequest) -> Result<Value, ProviderError> {
    let origin = Origin {
        route: ROUTE.to_string(),
        model: model.to_string(),
    };
    let input = input_items(request, &origin);

    let mut body = Map::new();
    body.insert("model".to_string(), json!(model));
    body.insert("store".to_string(), json!(false));
    body.insert("stream".to_string(), json!(true));
    body.insert("instructions".to_string(), json!(request.system_prompt));
    let replayed_reasoning = input
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
    body.insert("input".to_string(), Value::Array(input));
    let tools = tool_declarations(&request.tools);
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    body.insert(
        "text".to_string(),
        json!({ "verbosity": verbosity(&request.options)? }),
    );
    if let Some(key) = clamped_cache_key(&request.options) {
        body.insert("prompt_cache_key".to_string(), json!(key));
    }

    let mut include_reasoning = replayed_reasoning;
    if let Some(effort) = request.options.reasoning_effort {
        body.insert(
            "reasoning".to_string(),
            json!({ "effort": reasoning_effort(effort)?, "summary": "auto" }),
        );
        include_reasoning = true;
    }
    if include_reasoning {
        body.insert(
            "include".to_string(),
            json!(["reasoning.encrypted_content"]),
        );
    }
    Ok(Value::Object(body))
}

fn input_items(request: &ProviderRequest, origin: &Origin) -> Vec<Value> {
    let mut items = Vec::new();
    // call_id -> was the call freeform (`ToolInput::Text`). A tool result is
    // answered with the matching output item kind; a result without a matching
    // earlier call is assumed to be a function result.
    let mut custom_calls: HashMap<String, bool> = HashMap::new();
    for item in &request.history {
        match item {
            Item::User { text } | Item::Inbox { text, .. } => {
                items.push(message_item("user", text));
            }
            Item::Assistant(assistant) => {
                for block in &assistant.blocks {
                    match block {
                        AssistantBlock::Text { text } if !text.is_empty() => {
                            items.push(message_item("assistant", text));
                        }
                        AssistantBlock::Text { .. } => {}
                        AssistantBlock::ToolCall(call) => match &call.input {
                            ToolInput::Json(raw) => {
                                custom_calls.insert(call.call_id.clone(), false);
                                items.push(json!({
                                    "type": "function_call",
                                    "call_id": call.call_id,
                                    "name": call.name,
                                    "arguments": raw,
                                }));
                            }
                            ToolInput::Text(raw) => {
                                custom_calls.insert(call.call_id.clone(), true);
                                items.push(json!({
                                    "type": "custom_tool_call",
                                    "call_id": call.call_id,
                                    "name": call.name,
                                    "input": raw,
                                }));
                            }
                        },
                        AssistantBlock::Reasoning { replay, .. } => {
                            if let Some(replayed) = replayed_reasoning(replay.as_ref(), origin) {
                                items.push(replayed);
                            }
                        }
                    }
                }
            }
            Item::ToolResult(result) => {
                let custom = custom_calls.get(&result.call_id).copied().unwrap_or(false);
                let kind = if custom {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                items.push(json!({
                    "type": kind,
                    "call_id": result.call_id,
                    "output": result.content,
                }));
            }
        }
    }
    items
}

fn message_item(role: &str, text: &str) -> Value {
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

/// Replay a reasoning block only for the origin that produced it, at version 1.
/// A foreign or stale block is dropped rather than downgraded to text.
fn replayed_reasoning(replay: Option<&ReplayData>, origin: &Origin) -> Option<Value> {
    let replay = replay?;
    if &replay.origin != origin || replay.version != 1 {
        return None;
    }
    let encrypted = replay
        .payload
        .get("encrypted_content")
        .and_then(Value::as_str)?;
    Some(json!({
        "type": "reasoning",
        "encrypted_content": encrypted,
        "summary": [],
    }))
}

fn tool_declarations(tools: &[ToolDeclaration]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| match &tool.kind {
            DeclarationKind::Function { input_schema } => json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": input_schema,
                "strict": false,
            }),
            DeclarationKind::Freeform {
                grammar: Some(grammar),
            } => json!({
                "type": "custom",
                "name": tool.name,
                "description": tool.description,
                "format": {
                    "type": "grammar",
                    "syntax": grammar.syntax,
                    "definition": grammar.definition,
                },
            }),
            DeclarationKind::Freeform { grammar: None } => json!({
                "type": "custom",
                "name": tool.name,
                "description": tool.description,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use p1_contracts::history::{AssistantItem, ToolCall, ToolResultItem, ToolStatus};
    use p1_contracts::tool::Grammar;

    use super::*;

    fn user(text: &str) -> Item {
        Item::User {
            text: text.to_string(),
        }
    }

    fn assistant(blocks: Vec<AssistantBlock>) -> Item {
        Item::Assistant(AssistantItem {
            origin: Origin {
                route: ROUTE.to_string(),
                model: "gpt-test".to_string(),
            },
            blocks,
        })
    }

    fn request_with(history: Vec<Item>, tools: Vec<ToolDeclaration>) -> ProviderRequest {
        ProviderRequest {
            system_prompt: "You are a coding assistant.".to_string(),
            history,
            tools,
            options: ModelOptions::default(),
        }
    }

    fn credential(account_id: Option<&str>) -> Credential {
        Credential {
            bearer: "SENTINEL-ACCESS".to_string(),
            account_id: account_id.map(str::to_string),
        }
    }

    #[test]
    fn resolves_the_default_endpoint() {
        assert_eq!(
            resolve_base_url(DEFAULT_BASE_URL).unwrap(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_base_url("https://chatgpt.com/backend-api/").unwrap(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_base_url("https://example.test/codex").unwrap(),
            "https://example.test/codex/responses"
        );
        assert_eq!(
            resolve_base_url("https://example.test/codex/responses").unwrap(),
            "https://example.test/codex/responses"
        );
    }

    #[test]
    fn rejects_invalid_codex_base_url() {
        assert!(resolve_base_url("not a url").is_err());
        assert!(resolve_base_url("").is_err());
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn builds_the_exact_header_set() {
        let headers = build_headers(&credential(Some("acct_1")), None).unwrap();
        assert_eq!(
            headers,
            vec![
                (
                    "Authorization".to_string(),
                    "Bearer SENTINEL-ACCESS".to_string()
                ),
                ("chatgpt-account-id".to_string(), "acct_1".to_string()),
                ("originator".to_string(), "p1".to_string()),
                (
                    "User-Agent".to_string(),
                    format!("p1/{}", env!("CARGO_PKG_VERSION"))
                ),
                (
                    "OpenAI-Beta".to_string(),
                    "responses=experimental".to_string()
                ),
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "text/event-stream".to_string()),
            ]
        );
    }

    #[test]
    fn build_headers_requires_an_account_id() {
        let error = build_headers(&credential(None), None).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert!(!error.message.contains("SENTINEL-ACCESS"));
    }

    #[test]
    fn cache_key_is_sent_as_session_and_conversation_headers() {
        let mut request = request_with(vec![user("hi")], Vec::new());
        request.options.cache_key = Some("agent-a-key".to_string());
        let body = build_request("gpt-test", &request).unwrap();
        let key = clamped_cache_key(&request.options).unwrap();
        let headers = build_headers(&credential(Some("acct_1")), Some(&key)).unwrap();
        assert_eq!(header(&headers, "session_id"), Some("agent-a-key"));
        assert_eq!(header(&headers, "conversation_id"), Some("agent-a-key"));
        assert_eq!(
            header(&headers, "session_id"),
            body["prompt_cache_key"].as_str()
        );
    }

    #[test]
    fn cache_key_headers_are_absent_without_a_cache_key() {
        let headers = build_headers(&credential(Some("acct_1")), None).unwrap();
        assert!(header(&headers, "session_id").is_none());
        assert!(header(&headers, "conversation_id").is_none());
    }

    #[test]
    fn cache_key_headers_use_the_clamped_value() {
        let mut request = request_with(vec![user("hi")], Vec::new());
        request.options.cache_key = Some(format!("{}tail", "å".repeat(70)));
        let body = build_request("gpt-test", &request).unwrap();
        let key = clamped_cache_key(&request.options).unwrap();
        assert_eq!(key.chars().count(), 64);
        let headers = build_headers(&credential(Some("acct_1")), Some(&key)).unwrap();
        for name in ["session_id", "conversation_id"] {
            let value = header(&headers, name).unwrap();
            assert_eq!(value.chars().count(), 64, "{name} must be clamped too");
            assert_eq!(value, "å".repeat(64));
            assert_eq!(Some(value), body["prompt_cache_key"].as_str());
        }
    }

    #[test]
    fn cache_key_headers_never_carry_a_different_agents_key() {
        let mut first = request_with(vec![user("hi")], Vec::new());
        first.options.cache_key = Some("agent-a-key".to_string());
        let mut second = request_with(vec![user("hi")], Vec::new());
        second.options.cache_key = Some("agent-b-key".to_string());

        let first_key = clamped_cache_key(&first.options).unwrap();
        let second_key = clamped_cache_key(&second.options).unwrap();
        let first_headers = build_headers(&credential(Some("acct_1")), Some(&first_key)).unwrap();
        let second_headers = build_headers(&credential(Some("acct_2")), Some(&second_key)).unwrap();

        assert_eq!(header(&first_headers, "session_id"), Some("agent-a-key"));
        assert_eq!(
            header(&first_headers, "conversation_id"),
            Some("agent-a-key")
        );
        assert_ne!(header(&first_headers, "session_id"), Some("agent-b-key"));
        assert_ne!(
            header(&first_headers, "conversation_id"),
            Some("agent-b-key")
        );
        assert_eq!(header(&second_headers, "session_id"), Some("agent-b-key"));
        assert_eq!(
            header(&second_headers, "conversation_id"),
            Some("agent-b-key")
        );
        assert_ne!(header(&second_headers, "session_id"), Some("agent-a-key"));
    }

    #[test]
    fn default_body_is_exact() {
        let request = request_with(vec![user("hello")], Vec::new());
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "gpt-test",
                "store": false,
                "stream": true,
                "instructions": "You are a coding assistant.",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hello" }],
                }],
                "text": { "verbosity": "low" },
            })
        );
        assert!(body.get("tools").is_none(), "tools omitted when empty");
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn builds_from_conversation_with_assistant_text() {
        let request = request_with(
            vec![
                user("hello"),
                assistant(vec![AssistantBlock::Text {
                    text: "hi".to_string(),
                }]),
                Item::Inbox {
                    kind: p1_contracts::InboxKind::Steering,
                    text: "continue".to_string(),
                },
            ],
            Vec::new(),
        );
        let body = build_request("gpt-test", &request).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "hi");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][0]["text"], "continue");
    }

    #[test]
    fn empty_assistant_text_blocks_are_skipped() {
        let request = request_with(
            vec![assistant(vec![AssistantBlock::Text {
                text: String::new(),
            }])],
            Vec::new(),
        );
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(body["input"], json!([]));
    }

    #[test]
    fn builds_function_call_and_output_from_history() {
        let request = request_with(
            vec![
                assistant(vec![AssistantBlock::ToolCall(ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    input: ToolInput::Json(r#"{"path":"a.txt"}"#.to_string()),
                })]),
                Item::ToolResult(ToolResultItem {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    status: ToolStatus::Ok,
                    content: "file text".to_string(),
                }),
            ],
            Vec::new(),
        );
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(
            body["input"],
            json!([
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{\"path\":\"a.txt\"}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "file text",
                },
            ])
        );
    }

    #[test]
    fn custom_tool_call_round_trips_through_custom_output() {
        let declaration = ToolDeclaration {
            name: "apply_patch".to_string(),
            description: "Apply a patch".to_string(),
            kind: DeclarationKind::Freeform {
                grammar: Some(Grammar {
                    syntax: "lark".to_string(),
                    definition: "start: /.*/".to_string(),
                }),
            },
        };
        let request = request_with(
            vec![
                assistant(vec![AssistantBlock::ToolCall(ToolCall {
                    call_id: "call_patch".to_string(),
                    name: "apply_patch".to_string(),
                    input: ToolInput::Text("*** Begin Patch".to_string()),
                })]),
                Item::ToolResult(ToolResultItem {
                    call_id: "call_patch".to_string(),
                    name: "apply_patch".to_string(),
                    status: ToolStatus::Ok,
                    content: "applied".to_string(),
                }),
            ],
            vec![declaration],
        );
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(
            body["tools"][0],
            json!({
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch",
                "format": {
                    "type": "grammar",
                    "syntax": "lark",
                    "definition": "start: /.*/",
                },
            })
        );
        assert_eq!(
            body["input"][0],
            json!({
                "type": "custom_tool_call",
                "call_id": "call_patch",
                "name": "apply_patch",
                "input": "*** Begin Patch",
            })
        );
        assert_eq!(
            body["input"][1],
            json!({
                "type": "custom_tool_call_output",
                "call_id": "call_patch",
                "output": "applied",
            })
        );
    }

    #[test]
    fn unmatched_tool_result_defaults_to_function_output() {
        let request = request_with(
            vec![Item::ToolResult(ToolResultItem {
                call_id: "orphan".to_string(),
                name: "read".to_string(),
                status: ToolStatus::Error,
                content: "nope".to_string(),
            })],
            Vec::new(),
        );
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(body["input"][0]["type"], "function_call_output");
    }

    #[test]
    fn invalid_function_arguments_are_preserved_raw() {
        let request = request_with(
            vec![assistant(vec![AssistantBlock::ToolCall(ToolCall {
                call_id: "call_1".to_string(),
                name: "read".to_string(),
                input: ToolInput::Json(r#"{"path": "#.to_string()),
            })])],
            Vec::new(),
        );
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(body["input"][0]["arguments"], r#"{"path": "#);
    }

    #[test]
    fn freeform_without_grammar_omits_format() {
        let declaration = ToolDeclaration {
            name: "apply_patch".to_string(),
            description: "Apply a patch".to_string(),
            kind: DeclarationKind::Freeform { grammar: None },
        };
        let request = request_with(Vec::new(), vec![declaration]);
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(
            body["tools"][0],
            json!({
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch",
            })
        );
    }

    #[test]
    fn function_tools_are_declared_strict_false() {
        let declaration = ToolDeclaration {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            kind: DeclarationKind::Function {
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        };
        let request = request_with(Vec::new(), vec![declaration]);
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(
            body["tools"][0],
            json!({
                "type": "function",
                "name": "read",
                "description": "Read a file",
                "parameters": { "type": "object", "properties": {} },
                "strict": false,
            })
        );
    }

    #[test]
    fn reasoning_effort_adds_reasoning_and_include() {
        for (effort, wire) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
        ] {
            let mut request = request_with(vec![user("hi")], Vec::new());
            request.options.reasoning_effort = Some(effort);
            let body = build_request("gpt-test", &request).unwrap();
            assert_eq!(
                body["reasoning"],
                json!({ "effort": wire, "summary": "auto" })
            );
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
    }

    #[test]
    fn build_request_rejects_extra_high_and_max() {
        for effort in [Effort::ExtraHigh, Effort::Max] {
            let mut request = request_with(vec![user("hi")], Vec::new());
            request.options.reasoning_effort = Some(effort);
            let error = build_request("gpt-test", &request).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        }
    }

    #[test]
    fn prompt_cache_key_is_clamped_to_64_chars() {
        let mut request = request_with(vec![user("hi")], Vec::new());
        request.options.cache_key = Some(format!("{}tail", "å".repeat(70)));
        let body = build_request("gpt-test", &request).unwrap();
        let key = body["prompt_cache_key"].as_str().unwrap();
        assert_eq!(key.chars().count(), 64);
        assert_eq!(key, "å".repeat(64));
    }

    #[test]
    fn reasoning_replay_requires_this_origin_and_version() {
        let encrypted = "enc-1";
        let matching = ReplayData {
            origin: Origin {
                route: ROUTE.to_string(),
                model: "gpt-test".to_string(),
            },
            version: 1,
            payload: json!({ "type": "reasoning", "encrypted_content": encrypted }),
        };
        let foreign_route = ReplayData {
            origin: Origin {
                route: "other-route".to_string(),
                model: "gpt-test".to_string(),
            },
            ..matching.clone()
        };
        let foreign_model = ReplayData {
            origin: Origin {
                route: ROUTE.to_string(),
                model: "other-model".to_string(),
            },
            ..matching.clone()
        };
        let old_version = ReplayData {
            version: 2,
            ..matching.clone()
        };

        for replay in [foreign_route, foreign_model, old_version] {
            let request = request_with(
                vec![assistant(vec![AssistantBlock::Reasoning {
                    text: "summary".to_string(),
                    replay: Some(replay),
                }])],
                Vec::new(),
            );
            let body = build_request("gpt-test", &request).unwrap();
            assert_eq!(body["input"], json!([]), "foreign replay must be dropped");
            assert!(body.get("include").is_none());
        }

        let request = request_with(
            vec![assistant(vec![AssistantBlock::Reasoning {
                text: "summary".to_string(),
                replay: Some(matching),
            }])],
            Vec::new(),
        );
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(
            body["input"],
            json!([{
                "type": "reasoning",
                "encrypted_content": "enc-1",
                "summary": [],
            }])
        );
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "replayed reasoning must request encrypted content"
        );
    }

    #[test]
    fn reasoning_without_replay_is_dropped() {
        let request = request_with(
            vec![assistant(vec![AssistantBlock::Reasoning {
                text: "summary text must not become assistant text".to_string(),
                replay: None,
            }])],
            Vec::new(),
        );
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(body["input"], json!([]));
    }

    #[test]
    fn verbosity_native_option_overrides_text_verbosity() {
        let mut request = request_with(vec![user("hi")], Vec::new());
        request
            .options
            .native
            .insert(VERBOSITY_KEY.to_string(), json!("high"));
        let body = build_request("gpt-test", &request).unwrap();
        assert_eq!(body["text"], json!({ "verbosity": "high" }));
    }

    #[test]
    fn validate_rejects_unsupported_options() {
        let max_output = ModelOptions {
            max_output_tokens: Some(10),
            ..ModelOptions::default()
        };
        assert_eq!(
            validate(&max_output).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );

        let effort = ModelOptions {
            reasoning_effort: Some(Effort::Max),
            ..ModelOptions::default()
        };
        assert_eq!(
            validate(&effort).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );

        let mut options = ModelOptions::default();
        options
            .native
            .insert("openai-responses.mystery".to_string(), json!(1));
        assert_eq!(
            validate(&options).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );

        let mut options = ModelOptions::default();
        options
            .native
            .insert("openai-responses.verbosity".to_string(), json!("loud"));
        assert_eq!(
            validate(&options).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn validate_ignores_other_native_namespaces() {
        let mut options = ModelOptions::default();
        options
            .native
            .insert("anthropic-messages.mystery".to_string(), json!(1));
        options
            .native
            .insert("openai-responses.verbosity".to_string(), json!("medium"));
        validate(&options).unwrap();
    }
}
