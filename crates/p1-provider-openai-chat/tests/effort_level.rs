//! The `effort-level` dialect (ADR-0202): OpenAI's Chat Completions encoding. The resolved
//! effort is the one `reasoning_effort` field and nothing of the thinking-mode wires goes
//! out: no `thinking` object, no `reasoning_content`, the output cap as
//! `max_completion_tokens`, the cache key as `prompt_cache_key`, and `store: false`.

use std::sync::Arc;

use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, DeclarationKind, Effort, Item, ModelOptions,
    Provider, ProviderError, ProviderErrorKind, ProviderRequest, ToolCall, ToolDeclaration,
    ToolInput,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_conformance::fixtures::chat as fixtures;
use p1_provider_conformance::{RouteFixtures, RouteUnderTest, run_all_without_reasoning_replay};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai_chat::{
    ChatDialect, ChatLimits, ChatProvider, ChatRoute, build_request, lower_request,
    validate_composition,
};
use serde_json::{Value, json};

const MODEL: &str = "gpt-5.6-sol";
const BEARER: &str = "EFFORT-LEVEL-FAKE-BEARER";

/// Copied from `profiles/gpt-5.6-sol.toml` on 2026-10-11; independent of it now, so a moved
/// or edited file cannot break this target at compile time (#844).
const GPT_5_6_SOL: &str = r#"
id       = "gpt-5.6-sol"
revision = 2
model_id = "gpt-5.6-sol"
family   = "gpt"
thinking = "effort-level"

efforts = ["low", "medium", "high", "extra_high", "max"]
"#;

struct Fixed;

impl CredentialSource for Fixed {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: None,
            })
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}

/// `routes/openai-chat.toml` as the host composes it: p1's own user agent, no session
/// header, no client identity.
fn route() -> ChatRoute {
    ChatRoute {
        origin_route: "openai-chat/openai-chat".into(),
        endpoint: "https://api.openai.com/v1/chat/completions".into(),
        headers: vec![("user-agent".into(), "p1/test".into())],
        session_header: None,
        dialect: ChatDialect::EffortLevel,
        client_identity: None,
        limits: ChatLimits::default(),
    }
}

fn profile() -> ModelProfile {
    ModelProfile::from_toml("gpt-5.6-sol", GPT_5_6_SOL).expect("the shipped profile text")
}

/// A profile that lists `off` too, the one case `none` is reachable (ADR-0170).
fn profile_with_off() -> ModelProfile {
    let mut profile = profile();
    profile.efforts.insert(0, Effort::Off);
    profile
}

fn tool() -> ToolDeclaration {
    ToolDeclaration {
        name: "read".into(),
        description: "Read a file".into(),
        kind: DeclarationKind::Function {
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        },
    }
}

fn request(effort: Option<Effort>, cache_key: Option<&str>, cap: Option<u32>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![tool()],
        options: ModelOptions {
            reasoning_effort: effort,
            max_output_tokens: cap,
            cache_key: cache_key.map(str::to_string),
            ..ModelOptions::default()
        },
    }
}

fn built(profile: &ModelProfile, request: &ProviderRequest) -> Value {
    build_request(&route(), MODEL, profile, request).expect("the request builds")
}

#[test]
fn the_resolved_effort_is_the_one_reasoning_field_and_no_thinking_object_goes_out() {
    let cases = [
        (None, None),
        (Some(Effort::Low), Some("low")),
        (Some(Effort::Medium), Some("medium")),
        (Some(Effort::High), Some("high")),
        (Some(Effort::ExtraHigh), Some("xhigh")),
        (Some(Effort::Max), Some("max")),
    ];
    for (effort, wire) in cases {
        let body = built(&profile(), &request(effort, None, None));
        assert_eq!(
            body.get("reasoning_effort").and_then(Value::as_str),
            wire,
            "{effort:?}"
        );
        assert!(body.get("thinking").is_none(), "{effort:?}: {body}");
        assert!(body.get("tool_stream").is_none(), "{effort:?}: {body}");
        assert_eq!(body["store"], json!(false), "{effort:?}");
        assert_eq!(body["model"], json!(MODEL));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(
            body["tools"],
            json!([{"type": "function", "function": {
                "name": "read",
                "description": "Read a file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
            }}])
        );
    }
    // `off` is the wire's `none`, reached only through a profile that lists it.
    let body = built(&profile_with_off(), &request(Some(Effort::Off), None, None));
    assert_eq!(body["reasoning_effort"], json!("none"));
    assert!(body.get("thinking").is_none(), "{body}");
    let error = build_request(
        &route(),
        MODEL,
        &profile(),
        &request(Some(Effort::Off), None, None),
    )
    .expect_err("the shipped profile lists no off");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

#[test]
fn the_cap_is_max_completion_tokens_and_the_cache_key_is_prompt_cache_key() {
    let body = built(&profile(), &request(None, Some("session-key"), Some(2048)));
    assert_eq!(body["max_completion_tokens"], json!(2048));
    assert!(body.get("max_tokens").is_none(), "{body}");
    assert_eq!(body["prompt_cache_key"], json!("session-key"));
    // The dialect takes the key without a session header, and sends no header for it.
    let lowered = lower_request(&route(), MODEL, &profile(), &request(None, Some("k"), None))
        .expect("the request lowers");
    assert_eq!(
        lowered.headers,
        vec![
            ("user-agent".to_string(), "p1/test".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "text/event-stream".to_string()),
        ]
    );
    assert_eq!(
        route().describe(MODEL).cache_key,
        p1_contracts::CacheKeySupport::Optional
    );
    let body = built(&profile(), &request(None, None, None));
    assert!(body.get("prompt_cache_key").is_none(), "{body}");
    assert!(body.get("max_completion_tokens").is_none(), "{body}");
}

#[test]
fn an_assistant_message_is_content_and_tool_calls_and_never_reasoning_content() {
    let route = route();
    let origin = route.origin(MODEL);
    let mut request = request(Some(Effort::High), None, None);
    // Own-origin reasoning with replay data, as the parser would record it on a wire that
    // sent some, plus a text block and a call: the chat wires replay it, this one drops it.
    request.history.push(Item::Assistant(AssistantItem {
        origin: origin.clone(),
        blocks: vec![
            AssistantBlock::Reasoning {
                text: "display".into(),
                replay: Some(p1_provider_openai_chat::encode(&origin, "exact-replay")),
            },
            AssistantBlock::Text {
                text: "reading".into(),
            },
            AssistantBlock::ToolCall(ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                input: ToolInput::Json(r#"{"path":"a"}"#.into()),
            }),
        ],
    }));
    request
        .history
        .push(Item::ToolResult(p1_contracts::ToolResultItem {
            call_id: "call_1".into(),
            name: "read".into(),
            content: "contents".into(),
            status: p1_contracts::ToolStatus::Ok,
        }));
    let body = build_request(&route, MODEL, &profile(), &request).expect("the request builds");
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "reading", "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"a\"}"},
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "contents"},
        ])
    );
    assert!(!body.to_string().contains("exact-replay"), "{body}");
    assert!(!body.to_string().contains("reasoning_content"), "{body}");
}

#[test]
fn the_dialect_composes_effort_level_profiles_only_and_needs_no_default_effort() {
    let shipped = profile();
    assert_eq!(shipped.default_effort, None);
    validate_composition(&route(), MODEL, &shipped).expect("the shipped GPT profile composes");
    for (policy, name) in [
        (ThinkingPolicy::Enabled, "enabled"),
        (ThinkingPolicy::Preserved, "preserved"),
        (ThinkingPolicy::Budget, "budget"),
    ] {
        let mut profile = profile();
        profile.thinking = policy;
        profile.default_effort = Some(Effort::High);
        if policy == ThinkingPolicy::Budget {
            profile.thinking_budgets = profile
                .efforts
                .iter()
                .map(|effort| (*effort, 32_768))
                .collect();
        }
        profile
            .validate()
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let error = validate_composition(&route(), MODEL, &profile).expect_err(name);
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{name}");
        assert!(
            error.message.contains("effort-level"),
            "{name}: {}",
            error.message
        );
        assert!(error.message.contains(name), "{name}: {}", error.message);
    }
    // The thinking-mode dialects keep refusing the shipped GPT profile.
    for dialect in [
        ChatDialect::ThinkingWithReasoningAlias,
        ChatDialect::RetainedThinking,
    ] {
        let mut route = route();
        route.dialect = dialect;
        let error = validate_composition(&route, MODEL, &shipped).expect_err("refused");
        assert!(
            error.message.contains("effort-level"),
            "{dialect:?}: {}",
            error.message
        );
    }
}

fn provider(transport: ScriptedTransport) -> Arc<dyn Provider> {
    Arc::new(
        ChatProvider::new(
            route(),
            MODEL,
            Arc::new(profile()),
            Arc::new(transport),
            Arc::new(Fixed),
        )
        .expect("the route and profile compose"),
    )
}

fn follow_up(request: &ProviderRequest) -> Value {
    build_request(&route(), MODEL, &profile(), request).expect("the follow-up builds")
}

/// A request this dialect must reject: an effort the profile does not list.
fn invalid() -> ProviderRequest {
    request(Some(Effort::Off), None, None)
}

/// The shared suite, without the reasoning replay: this wire returns no reasoning.
#[test]
fn the_effort_level_dialect_passes_the_shared_suite_without_reasoning_replay() {
    run_all_without_reasoning_replay(&RouteUnderTest {
        name: "openai-chat/openai-chat",
        build: provider,
        max_retries: 3,
        fixtures: RouteFixtures {
            text_turn: fixtures::TEXT_TURN,
            tool_call_turn: fixtures::TOOL_CALL_TURN,
            two_tool_calls: fixtures::TWO_TOOL_CALLS,
            truncated_tool_call: fixtures::TRUNCATED_TOOL_CALL,
            invalid_tool_json: fixtures::INVALID_TOOL_JSON,
            error_event: fixtures::ERROR_EVENT,
            no_usage: fixtures::NO_USAGE,
            reasoning_turn: fixtures::REASONING_TURN,
            events_after_terminal: fixtures::EVENTS_AFTER_TERMINAL,
        },
        follow_up_request: follow_up,
        fake_bearer: BEARER,
        invalid_request: invalid,
    });
}
