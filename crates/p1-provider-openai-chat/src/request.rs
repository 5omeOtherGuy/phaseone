use crate::replay::{self, Replay};
use crate::{ChatDialect, ChatRoute};
use p1_contracts::{
    AssistantBlock, AssistantItem, DeclarationKind, Effort, Item, Origin, ProviderError,
    ProviderErrorKind, ProviderRequest, ToolInput,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use serde_json::{Value, json};

pub(crate) fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

/// Refuse what the composed route cannot carry, before a run starts: the
/// provider's `validate`, a function of the composition so a component shares it.
pub fn validate(
    route: &ChatRoute,
    model: &str,
    profile: &ModelProfile,
    request: &ProviderRequest,
) -> Result<(), ProviderError> {
    if request
        .tools
        .iter()
        .any(|t| matches!(t.kind, DeclarationKind::Freeform { .. }))
    {
        return Err(invalid(
            "chat subscription routes support function tools only",
        ));
    }
    profile.resolve_effort(request.options.reasoning_effort)?;
    for limit in [profile.max_output_tokens, route.limits.max_output_tokens]
        .into_iter()
        .flatten()
    {
        if request
            .options
            .max_output_tokens
            .is_some_and(|cap| cap > limit)
        {
            return Err(invalid("output cap exceeds the model or route limit"));
        }
    }
    if request.options.max_output_tokens == Some(0) {
        return Err(invalid("output cap must be positive"));
    }
    if let Some(key) = &request.options.cache_key {
        if route.session_header.is_none() {
            return Err(invalid("chat route does not support an explicit cache key"));
        }
        if key.is_empty() || key.len() > 256 || !key.bytes().all(|b| (33..=126).contains(&b)) {
            return Err(invalid(
                "cache key must be 1..256 printable ASCII bytes without spaces",
            ));
        }
    }
    // Namespaces this adapter owns: the generic `openai-chat.` one and this
    // route's own id. No native option is known in this slice, so any key there
    // is an error.
    let own_namespaces = [
        format!("{}.", route.origin_route),
        "openai-chat.".to_string(),
    ];
    for key in request.options.native.keys() {
        if own_namespaces.iter().any(|prefix| key.starts_with(prefix)) {
            return Err(invalid("unknown native option in chat route namespace"));
        }
        // Another adapter's namespace is an explicit preference this route
        // cannot consume; dropping it silently is the portability trap the
        // adapter/route split exists to remove (ADR-0039).
        if FOREIGN_NATIVE_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            return Err(invalid(&format!(
                "option \"{key}\" is not consumed by route \"{}\" (adapter openai-chat): \
                 it belongs to another adapter's namespace",
                route.origin_route
            )));
        }
    }
    // ADR-0049: the chat wire carries every tool input (a freeform call from
    // another route becomes a function call whose arguments are `{"input": …}`) and
    // drops foreign reasoning. Whatever is left that cannot be lowered is refused
    // here, by the ONE lowering, before anything is sent.
    let origin = route.origin(model);
    for item in &request.history {
        if let Item::Assistant(item) = item {
            lower_item(&origin, item)?;
        }
    }
    Ok(())
}

/// Namespaces the OTHER compiled adapters own inside `ModelOptions::native`.
/// Keys in no adapter's namespace keep their meaning: ignored.
const FOREIGN_NATIVE_PREFIXES: &[&str] = &["anthropic-messages.", "openai-responses."];

/// One assistant item as the chat wire carries it.
struct LoweredItem {
    text: String,
    reasoning: String,
    has_reasoning: bool,
    calls: Vec<Value>,
}

/// The ONE lowering of one assistant item. Text is joined, an own-origin reasoning
/// replay is replayed byte-exact, a FOREIGN reasoning block is dropped, never
/// rendered as assistant text, and a tool call becomes a function call. A
/// `ToolInput::Text` — a freeform call made on a route that has such a shape — has
/// no freeform shape here, so its arguments are the JSON object
/// `{"input": <raw text>}` (ADR-0049, model-selection.md §3). Our own replay data in a
/// layout this build does not read is refused with a sentence naming the item, its
/// origin and both versions. `validate` and `build_request` share this, so `validate`
/// can never accept a request the builder would reject.
fn lower_item(origin: &Origin, item: &AssistantItem) -> Result<LoweredItem, ProviderError> {
    let mut lowered = LoweredItem {
        text: String::new(),
        reasoning: String::new(),
        has_reasoning: false,
        calls: Vec::new(),
    };
    for block in &item.blocks {
        match block {
            AssistantBlock::Text { text } => lowered.text.push_str(text),
            AssistantBlock::Reasoning {
                replay: Some(data), ..
            } => match replay::decode(data, origin) {
                Replay::Foreign => {}
                Replay::Carried(part) => {
                    lowered.reasoning.push_str(part);
                    lowered.has_reasoning = true;
                }
                Replay::UnsupportedVersion { version } => {
                    return Err(invalid(&format!(
                        "cannot replay the reasoning block of the assistant item from {}/{}: \
                         its replay data is version {version}, this route reads version {}",
                        data.origin.route,
                        data.origin.model,
                        replay::REPLAY_VERSION
                    )));
                }
                Replay::UnsupportedPayload => {
                    return Err(invalid(&format!(
                        "cannot replay the reasoning block of the assistant item from {}/{}: \
                         its replay payload is not text",
                        data.origin.route, data.origin.model
                    )));
                }
            },
            // A reasoning block without replay data, and a foreign one: dropped
            // entirely (providers.md "Replay").
            AssistantBlock::Reasoning { .. } => {}
            AssistantBlock::ToolCall(call) => {
                let arguments = match &call.input {
                    ToolInput::Json(raw) => raw.clone(),
                    ToolInput::Text(raw) => json!({ "input": raw }).to_string(),
                };
                lowered.calls.push(json!({
                    "id": call.call_id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": arguments },
                }));
            }
        }
    }
    Ok(lowered)
}

/// Pure wire builder. Foreign reasoning is dropped, never rendered as assistant text.
pub fn build_request(
    route: &ChatRoute,
    model: &str,
    profile: &ModelProfile,
    request: &ProviderRequest,
) -> Result<Value, ProviderError> {
    crate::validate_composition(route, model, profile)?;
    validate(route, model, profile, request)?;
    let origin = route.origin(model);
    let mut messages = vec![json!({"role":"system", "content": request.system_prompt})];
    for item in &request.history {
        match item {
            Item::User { text } | Item::Inbox { text, .. } => {
                messages.push(json!({"role":"user", "content":text}))
            }
            Item::ToolResult(result) => messages.push(
                json!({"role":"tool", "tool_call_id":result.call_id, "content":result.content}),
            ),
            Item::Assistant(item) => {
                let LoweredItem {
                    text,
                    reasoning,
                    has_reasoning,
                    calls,
                } = lower_item(&origin, item)?;
                let mut message = json!({"role":"assistant","content":text});
                // A thinking-mode endpoint expects `reasoning_content` on every assistant
                // message that carries tool calls; a response that reasoned nothing still
                // has to replay the field, empty. Omitting it was answered with HTTP 400
                // (`invalid_request_error`) by some replicas of the DeepSeek route
                // (run ws-continuation, 2026-09-21).
                if has_reasoning || !calls.is_empty() {
                    message["reasoning_content"] = json!(reasoning);
                }
                if !calls.is_empty() {
                    message["tool_calls"] = json!(calls);
                }
                messages.push(message);
            }
        }
    }
    let reasoning_effort = match profile.resolve_effort(request.options.reasoning_effort)? {
        Some(Effort::Max) => "max",
        Some(Effort::Low) => "low",
        Some(_) => "high",
        None => return Err(invalid("chat profile has no default reasoning effort")),
    };
    let mut body = json!({"model":model,"messages":messages,"stream":true,"stream_options":{"include_usage":true},"reasoning_effort":reasoning_effort});
    body["thinking"] = match profile.thinking {
        ThinkingPolicy::Enabled => json!({"type":"enabled"}),
        ThinkingPolicy::Preserved => json!({"type":"enabled","clear_thinking":false}),
        ThinkingPolicy::EffortLevel | ThinkingPolicy::Budget => {
            return Err(invalid("chat dialect cannot express this thinking policy"));
        }
    };
    if let Some(cap) = request.options.max_output_tokens {
        body["max_tokens"] = json!(cap);
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| match &tool.kind {
            DeclarationKind::Function { input_schema } => json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":input_schema}}),
            _ => unreachable!("validated"),
        })
        .collect();
    // This adapter declares no tool of its own. The Zen free tier's gate wants
    // `bash` and `read` among the declared tools, but that is an environment's
    // business (`[[tools]] name = "bash"` / `"read"`), never the provider's: the
    // provider only translates the request's own tools to the wire.
    if !tools.is_empty() {
        if route.dialect == ChatDialect::RetainedThinking {
            body["tool_stream"] = json!(true);
        }
        body["tools"] = json!(tools);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{
        AssistantItem, ModelOptions, Origin, ReplayData, ToolCall, ToolDeclaration, ToolResultItem,
        ToolStatus,
    };
    fn request() -> ProviderRequest {
        ProviderRequest {
            system_prompt: "system".into(),
            history: vec![],
            tools: vec![],
            options: ModelOptions::default(),
        }
    }
    #[test]
    fn golden_tool_round_trip_and_subscription_options() {
        for retained in [false, true] {
            let route = crate::test_config::route(retained);
            let profile = crate::test_config::profile(retained);
            let mut r = request();
            let origin = route.origin("model");
            r.tools.push(ToolDeclaration {
                name: "read".into(),
                description: "Read".into(),
                kind: DeclarationKind::Function {
                    input_schema: json!({"type":"object"}),
                },
            });
            r.history = vec![
                Item::User {
                    text: "inspect".into(),
                },
                Item::Assistant(AssistantItem {
                    origin: origin.clone(),
                    blocks: vec![
                        AssistantBlock::Reasoning {
                            text: "display must not replay".into(),
                            replay: Some(ReplayData {
                                origin,
                                version: 1,
                                payload: json!("exact\n雪"),
                            }),
                        },
                        AssistantBlock::ToolCall(ToolCall {
                            call_id: "call".into(),
                            name: "read".into(),
                            input: ToolInput::Json("{ \"path\": \"a\" }".into()),
                        }),
                    ],
                }),
                Item::ToolResult(ToolResultItem {
                    call_id: "call".into(),
                    name: "read".into(),
                    status: ToolStatus::Error,
                    content: "not found".into(),
                }),
            ];
            r.options.max_output_tokens = Some(200);
            let body = build_request(&route, "model", &profile, &r).unwrap();
            let mut expected = json!({"model":"model","stream":true,"stream_options":{"include_usage":true},"reasoning_effort":"high","max_tokens":200,
                "thinking":if retained {json!({"type":"enabled","clear_thinking":false})}else{json!({"type":"enabled"})},
                "tools":[{"type":"function","function":{"name":"read","description":"Read","parameters":{"type":"object"}}}],
                "messages":[{"role":"system","content":"system"},{"role":"user","content":"inspect"},{"role":"assistant","content":"","reasoning_content":"exact\n雪","tool_calls":[{"id":"call","type":"function","function":{"name":"read","arguments":"{ \"path\": \"a\" }"}}]},{"role":"tool","tool_call_id":"call","content":"not found"}]
            });
            if route.dialect == ChatDialect::RetainedThinking {
                expected["tool_stream"] = json!(true);
            }
            assert_eq!(body, expected);
            let foreign = build_request(&route, "different-model", &profile, &r)
                .unwrap()
                .to_string();
            assert!(!foreign.contains("exact"));
            assert!(!foreign.contains("display must"));
        }
    }
    /// A response that called a tool without reasoning anything still replays the
    /// field, empty: a thinking-mode endpoint refused the request without it (HTTP 400
    /// `invalid_request_error`, run ws-continuation 2026-09-21). An assistant message
    /// WITHOUT tool calls and without reasoning stays as it was.
    #[test]
    fn a_tool_call_without_reasoning_replays_an_empty_reasoning_field() {
        for retained in [false, true] {
            let route = crate::test_config::route(retained);
            let profile = crate::test_config::profile(retained);
            let mut r = request();
            let origin = route.origin("model");
            r.history = vec![
                Item::User {
                    text: "inspect".into(),
                },
                Item::Assistant(AssistantItem {
                    origin: origin.clone(),
                    blocks: vec![AssistantBlock::Text {
                        text: "plain answer".into(),
                    }],
                }),
                Item::User {
                    text: "now read".into(),
                },
                Item::Assistant(AssistantItem {
                    origin,
                    blocks: vec![AssistantBlock::ToolCall(ToolCall {
                        call_id: "call".into(),
                        name: "read".into(),
                        input: ToolInput::Json("{}".into()),
                    })],
                }),
                Item::ToolResult(ToolResultItem {
                    call_id: "call".into(),
                    name: "read".into(),
                    status: ToolStatus::Ok,
                    content: "text".into(),
                }),
            ];
            let body = build_request(&route, "model", &profile, &r).unwrap();
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(
                messages[2],
                json!({"role":"assistant","content":"plain answer"})
            );
            assert_eq!(
                messages[4],
                json!({"role":"assistant","content":"","reasoning_content":"","tool_calls":[{"id":"call","type":"function","function":{"name":"read","arguments":"{}"}}]})
            );
        }
    }

    #[test]
    fn rejects_unrepresentable_options_and_tools() {
        let route = crate::test_config::route(false);
        let profile = crate::test_config::profile(false);
        let mut r = request();
        r.tools.push(ToolDeclaration {
            name: "patch".into(),
            description: String::new(),
            kind: DeclarationKind::Freeform { grammar: None },
        });
        assert!(build_request(&route, "m", &profile, &r).is_err());
        r.tools.clear();
        for effort in [Effort::Low, Effort::Medium, Effort::ExtraHigh] {
            r.options.reasoning_effort = Some(effort);
            assert!(build_request(&route, "m", &profile, &r).is_err());
        }
        r.options = ModelOptions::default();
        r.options
            .native
            .insert("openai-chat.typo".into(), json!(true));
        assert!(build_request(&route, "m", &profile, &r).is_err());
        r.options.native.clear();
        r.options.native.insert("other.option".into(), json!(true));
        assert!(build_request(&route, "m", &profile, &r).is_ok());
        r.options.cache_key = Some("bad\nheader".into());
        assert!(build_request(&route, "m", &profile, &r).is_err());
        r.options.cache_key = Some("valid".into());
        assert!(build_request(&route, "m", &profile, &r).is_ok());
        assert!(
            build_request(
                &crate::test_config::route(true),
                "m",
                &crate::test_config::profile(true),
                &r
            )
            .is_err()
        );
    }
    /// ADR-0049: a freeform call made on a route that has such a shape travels here
    /// as a function call whose arguments are `{"input": <raw text>}`, a JSON call is
    /// unchanged, and the foreign reasoning replay data stays dropped. `validate`
    /// accepts what the builder lowers.
    #[test]
    fn a_foreign_history_lowers_freeform_calls_and_drops_foreign_reasoning() {
        for retained in [false, true] {
            let route = crate::test_config::route(retained);
            let profile = crate::test_config::profile(retained);
            let foreign = Origin {
                route: "elsewhere/foreign-route".into(),
                model: "foreign-model".into(),
            };
            let mut r = request();
            r.history = vec![
                Item::User { text: "go".into() },
                Item::Assistant(AssistantItem {
                    origin: foreign.clone(),
                    blocks: vec![
                        AssistantBlock::Reasoning {
                            text: "foreign thought".into(),
                            replay: Some(ReplayData {
                                origin: foreign,
                                version: 1,
                                payload: json!("FOREIGN-REPLAY-LEAF"),
                            }),
                        },
                        AssistantBlock::ToolCall(ToolCall {
                            call_id: "call_json".into(),
                            name: "read".into(),
                            input: ToolInput::Json(r#"{"path":"a.txt"}"#.into()),
                        }),
                        AssistantBlock::ToolCall(ToolCall {
                            call_id: "call_text".into(),
                            name: "apply_patch".into(),
                            input: ToolInput::Text("*** Begin Patch ***".into()),
                        }),
                    ],
                }),
                Item::ToolResult(ToolResultItem {
                    call_id: "call_json".into(),
                    name: "read".into(),
                    status: ToolStatus::Ok,
                    content: "file body".into(),
                }),
                Item::ToolResult(ToolResultItem {
                    call_id: "call_text".into(),
                    name: "apply_patch".into(),
                    status: ToolStatus::Ok,
                    content: "patch applied".into(),
                }),
            ];
            validate(&route, "model", &profile, &r).expect("a foreign history lowers");
            let body = build_request(&route, "model", &profile, &r).unwrap();
            let calls = body["messages"][2]["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["id"], json!("call_json"));
            assert_eq!(
                calls[0]["function"]["arguments"],
                json!(r#"{"path":"a.txt"}"#)
            );
            assert_eq!(calls[1]["id"], json!("call_text"));
            assert_eq!(
                calls[1]["function"]["arguments"],
                json!(r#"{"input":"*** Begin Patch ***"}"#)
            );
            assert_eq!(body["messages"][3]["tool_call_id"], json!("call_json"));
            assert_eq!(body["messages"][4]["tool_call_id"], json!("call_text"));
            let rendered = body.to_string();
            assert!(!rendered.contains("FOREIGN-REPLAY-LEAF"), "{rendered}");
            assert!(!rendered.contains("foreign thought"), "{rendered}");
            // A foreign replay at another VERSION is dropped with the rest, never
            // refused: only this route's own data can be unreadable.
            let mut foreign_v2 = r.clone();
            if let Item::Assistant(item) = &mut foreign_v2.history[1]
                && let AssistantBlock::Reasoning {
                    replay: Some(data), ..
                } = &mut item.blocks[0]
            {
                data.version = 2;
            }
            assert!(validate(&route, "model", &profile, &foreign_v2).is_ok());
            assert!(build_request(&route, "model", &profile, &foreign_v2).is_ok());
        }
    }

    /// The one history this route still cannot lower is its OWN reasoning replay in
    /// a shape it does not read; the refusal names the item (ADR-0049).
    #[test]
    fn validate_refuses_an_own_reasoning_replay_it_cannot_read_and_names_it() {
        let route = crate::test_config::route(false);
        let profile = crate::test_config::profile(false);
        let origin = route.origin("model");
        for (version, payload, expected) in [
            (2u32, json!("reasoning"), "version 2"),
            (1, json!({ "parts": ["x"] }), "is not text"),
        ] {
            let mut r = request();
            r.history = vec![
                Item::User { text: "go".into() },
                Item::Assistant(AssistantItem {
                    origin: origin.clone(),
                    blocks: vec![AssistantBlock::Reasoning {
                        text: "shown".into(),
                        replay: Some(ReplayData {
                            origin: origin.clone(),
                            version,
                            payload,
                        }),
                    }],
                }),
            ];
            let error = validate(&route, "model", &profile, &r).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
            for part in [
                "assistant item from",
                origin.route.as_str(),
                origin.model.as_str(),
                expected,
            ] {
                assert!(error.message.contains(part), "{}: {part}", error.message);
            }
            assert_eq!(
                build_request(&route, "model", &profile, &r).unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn a_native_option_in_another_adapters_namespace_is_an_error() {
        let route = crate::test_config::route(false);
        let profile = crate::test_config::profile(false);
        for key in ["anthropic-messages.thinking", "openai-responses.verbosity"] {
            let mut r = request();
            r.options.native.insert(key.into(), json!(true));
            let error = build_request(&route, "m", &profile, &r).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{key}");
            for part in [
                format!("option \"{key}\""),
                "route \"openai-chat/opencode-go-subscription\"".to_string(),
                "(adapter openai-chat)".to_string(),
            ] {
                assert!(error.message.contains(&part), "{}: {}", error, part);
            }
        }
    }
    #[test]
    fn describe_reports_cache_key_support_from_the_session_header() {
        use crate::ChatProvider;
        use p1_contracts::{CacheKeySupport, Provider};
        let session = ChatProvider::new(
            crate::test_config::route(false),
            "m",
            std::sync::Arc::new(crate::test_config::profile(false)),
            std::sync::Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            std::sync::Arc::new(Fixed),
        )
        .unwrap();
        let no_session = ChatProvider::new(
            crate::test_config::route(true),
            "m",
            std::sync::Arc::new(crate::test_config::profile(true)),
            std::sync::Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            std::sync::Arc::new(Fixed),
        )
        .unwrap();
        assert_eq!(session.describe().cache_key, CacheKeySupport::Optional);
        assert_eq!(
            no_session.describe().cache_key,
            CacheKeySupport::Unsupported
        );
    }

    struct Fixed;
    impl p1_provider_http::CredentialSource for Fixed {
        fn access<'a>(
            &'a self,
        ) -> p1_contracts::BoxFuture<'a, Result<p1_provider_http::Credential, ProviderError>>
        {
            Box::pin(async {
                Ok(p1_provider_http::Credential {
                    bearer: "TEST".into(),
                    account_id: None,
                })
            })
        }
        fn refresh<'a>(
            &'a self,
            _rejected: &'a p1_provider_http::Credential,
        ) -> p1_contracts::BoxFuture<'a, Result<p1_provider_http::Credential, ProviderError>>
        {
            Box::pin(async {
                Ok(p1_provider_http::Credential {
                    bearer: "TEST".into(),
                    account_id: None,
                })
            })
        }
    }
    #[test]
    fn glm_effort_and_output_limits_match_the_documented_wire() {
        let mut r = request();
        for (effort, name) in [
            (Effort::Low, "low"),
            (Effort::High, "high"),
            (Effort::Max, "max"),
        ] {
            r.options.reasoning_effort = Some(effort);
            assert_eq!(
                build_request(
                    &crate::test_config::route(true),
                    "glm-5.3",
                    &crate::test_config::profile(true),
                    &r
                )
                .unwrap()["reasoning_effort"],
                name
            );
        }
        r.options.max_output_tokens = Some(131_072);
        assert!(
            build_request(
                &crate::test_config::route(true),
                "glm-5.3",
                &crate::test_config::profile(true),
                &r
            )
            .is_ok()
        );
        r.options.max_output_tokens = Some(131_073);
        assert!(
            build_request(
                &crate::test_config::route(true),
                "glm-5.3",
                &crate::test_config::profile(true),
                &r
            )
            .is_err()
        );
    }
}
