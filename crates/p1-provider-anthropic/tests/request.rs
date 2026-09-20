//! Pure request-builder, header and validation tests (golden whole bodies).

use std::sync::Arc;

use p1_contracts::history::{
    AssistantBlock, AssistantItem, InboxKind, Item, Origin, ReplayData, ToolCall, ToolInput,
    ToolResultItem, ToolStatus,
};
use p1_contracts::tool::{DeclarationKind, Grammar, ToolDeclaration};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, ModelOptions, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{
    AnthropicProvider, MessagesAccount, MessagesRoute, ROUTE, build_headers, build_request,
};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource, RetryPolicy, Transport};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// The route data the frozen expectations were recorded with: today's origin route
/// and endpoint. ADR-0039 step 4 moved them into `routes/anthropic-subscription.toml`;
/// these tests keep the same values, so their bodies stay byte-identical.
fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
    }
}

/// The profile the OLD rule selected for `model`: the three adaptive prefixes
/// (`claude-fable-5`, `claude-opus-5`, `claude-sonnet-5`) took an effort level, every
/// other name took the manual budget table. The mapping documents what the explicit
/// `profiles/claude-*.toml` records replaced; every expected body is unchanged.
fn profile(model: &str) -> ModelProfile {
    let effort_level = ["claude-fable-5", "claude-opus-5", "claude-sonnet-5"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
    let efforts = vec![
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::ExtraHigh,
        Effort::Max,
    ];
    ModelProfile {
        id: model.to_string(),
        revision: 1,
        model_id: model.to_string(),
        family: "claude".to_string(),
        thinking: if effort_level {
            ThinkingPolicy::EffortLevel
        } else {
            ThinkingPolicy::Budget
        },
        efforts,
        default_effort: None,
        thinking_budgets: if effort_level {
            BTreeMap::new()
        } else {
            [
                (Effort::Low, 4_096),
                (Effort::Medium, 10_240),
                (Effort::High, 20_480),
                (Effort::ExtraHigh, 32_768),
                (Effort::Max, 32_768),
            ]
            .into_iter()
            .collect()
        },
        context_tokens: None,
        max_output_tokens: None,
    }
}

/// The account these frozen headers belong to (the shipped route's).
fn account() -> MessagesAccount {
    MessagesAccount::ClaudeCodeSubscription
}

/// `build_request` over the route and the profile `model` selects.
fn build(model: &str, request: &ProviderRequest) -> Result<Value, ProviderError> {
    build_request(&route(), model, &profile(model), request)
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_string(),
    }
}

fn assistant(blocks: Vec<AssistantBlock>) -> Item {
    Item::Assistant(AssistantItem {
        origin: Origin {
            route: ROUTE.to_string(),
            model: "claude-sonnet-4-6".to_string(),
        },
        blocks,
    })
}

fn text_block(text: &str) -> AssistantBlock {
    AssistantBlock::Text {
        text: text.to_string(),
    }
}

fn tool_call(call_id: &str, name: &str, raw: &str) -> AssistantBlock {
    AssistantBlock::ToolCall(ToolCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        input: ToolInput::Json(raw.to_string()),
    })
}

fn result(call_id: &str, status: ToolStatus) -> Item {
    Item::ToolResult(ToolResultItem {
        call_id: call_id.to_string(),
        name: "read".to_string(),
        status,
        content: "body".to_string(),
    })
}

fn function_tool(name: &str) -> ToolDeclaration {
    ToolDeclaration {
        name: name.to_string(),
        description: format!("the {name} tool"),
        kind: DeclarationKind::Function {
            input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        },
    }
}

fn request(history: Vec<Item>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".to_string(),
        history,
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

fn with_effort(history: Vec<Item>, effort: Effort) -> ProviderRequest {
    let mut request = request(history);
    request.options.reasoning_effort = Some(effort);
    request
}

fn ephemeral() -> Value {
    json!({ "type": "ephemeral" })
}

#[test]
fn golden_request_whole_body() {
    let built = build("claude-sonnet-4-6", &request(vec![user("hi")])).unwrap();
    let expected = json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 32_000,
        "stream": true,
        "system": [
            { "type": "text", "text": IDENTITY },
            { "type": "text", "text": "SYS", "cache_control": ephemeral() },
        ],
        "messages": [
            { "role": "user", "content": [
                { "type": "text", "text": "hi", "cache_control": ephemeral() }
            ] }
        ],
    });
    assert_eq!(built, expected);
}

#[test]
fn empty_prompt_omits_the_second_system_block() {
    let mut request = request(vec![user("hi")]);
    request.system_prompt = String::new();
    let built = build("claude-sonnet-4-6", &request).unwrap();
    assert_eq!(
        built["system"],
        json!([{ "type": "text", "text": IDENTITY, "cache_control": ephemeral() }])
    );
}

#[test]
fn cache_control_marks_last_system_last_tool_and_last_user_block() {
    let mut request = request(vec![
        user("one"),
        assistant(vec![text_block("two")]),
        user("three"),
    ]);
    request.tools = vec![function_tool("read"), function_tool("grep")];
    let built = build("claude-sonnet-4-6", &request).unwrap();

    let system = built["system"].as_array().unwrap();
    assert!(system[0].get("cache_control").is_none());
    assert_eq!(system.last().unwrap()["cache_control"], ephemeral());

    let tools = built["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"], ephemeral());

    // The LAST user-role message is the only one marked, and only its last block.
    let messages = built["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert!(messages[0]["content"][0].get("cache_control").is_none());
    assert_eq!(messages[2]["content"].as_array().unwrap().len(), 1);
    assert_eq!(messages[2]["content"][0]["cache_control"], ephemeral());
}

#[test]
fn tools_are_omitted_when_empty_and_shaped_as_json_schema() {
    let bare = build("m", &request(vec![user("hi")])).unwrap();
    assert!(bare.get("tools").is_none());

    let mut with_tools = request(vec![user("hi")]);
    with_tools.tools = vec![function_tool("read")];
    let built = build("m", &with_tools).unwrap();
    assert_eq!(
        built["tools"][0],
        json!({
            "name": "read",
            "description": "the read tool",
            "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
            "cache_control": ephemeral(),
        })
    );
}

#[test]
fn no_thinking_when_no_reasoning_effort() {
    let built = build("claude-sonnet-4-6", &request(vec![user("hi")])).unwrap();
    assert!(built.get("thinking").is_none());
    assert!(built.get("output_config").is_none());
    assert_eq!(built["max_tokens"], json!(32_000));
}

#[test]
fn manual_thinking_budgets_and_the_output_margin() {
    for (effort, budget, expected_max) in [
        (Effort::Low, 4_096u64, 32_000u64),
        (Effort::Medium, 10_240, 32_000),
        (Effort::High, 20_480, 32_000),
        // The two highest budgets meet or exceed the default cap, so the output
        // cap is raised rather than the budget reduced.
        (Effort::ExtraHigh, 32_768, 40_960),
        (Effort::Max, 32_768, 40_960),
    ] {
        let built = build("claude-sonnet-4-6", &with_effort(vec![user("hi")], effort)).unwrap();
        assert_eq!(
            built["thinking"],
            json!({ "type": "enabled", "budget_tokens": budget }),
            "{effort:?}"
        );
        assert!(built.get("output_config").is_none(), "{effort:?}");
        assert_eq!(built["max_tokens"], json!(expected_max), "{effort:?}");
        let budget_value = built["thinking"]["budget_tokens"].as_u64().unwrap();
        let max_tokens = built["max_tokens"].as_u64().unwrap();
        assert!(budget_value >= 1_024, "{effort:?}");
        assert!(budget_value < max_tokens, "{effort:?}");
    }
}

#[test]
fn an_explicit_output_cap_below_the_thinking_budget_is_rejected_by_build_request() {
    // ADR-0039: an explicit cap the manual thinking budget would meet or exceed
    // is a rejected conflict, no longer silently raised to budget + 8192.
    let mut request = with_effort(vec![user("hi")], Effort::Low);
    request.options.max_output_tokens = Some(2_000);
    let error = build("claude-sonnet-4-6", &request).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    for part in [
        "max_output_tokens 2000".to_string(),
        "thinking budget 4096".to_string(),
        // The smallest cap that leaves room for the budget.
        "is 4097".to_string(),
    ] {
        assert!(error.message.contains(&part), "{}: {}", error, part);
    }
}

#[test]
fn an_explicit_output_cap_is_used_verbatim_without_thinking() {
    let mut request = request(vec![user("hi")]);
    request.options.max_output_tokens = Some(4_242);
    let built = build("claude-sonnet-4-6", &request).unwrap();
    assert_eq!(built["max_tokens"], json!(4_242));
}

#[test]
fn adaptive_models_use_effort_instead_of_a_budget() {
    for (effort, wire) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::ExtraHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let built = build("claude-sonnet-5", &with_effort(vec![user("hi")], effort)).unwrap();
        assert_eq!(
            built["thinking"],
            json!({ "type": "adaptive", "display": "summarized" }),
            "{effort:?}"
        );
        assert_eq!(
            built["output_config"],
            json!({ "effort": wire }),
            "{effort:?}"
        );
        assert!(built["thinking"].get("budget_tokens").is_none());
        assert_eq!(built["max_tokens"], json!(32_000));
    }

    // All three adaptive prefixes, and a non-adaptive lookalike family.
    for model in [
        "claude-fable-5",
        "claude-opus-5",
        "claude-sonnet-5-20260101",
    ] {
        let built = build(model, &with_effort(vec![user("hi")], Effort::High)).unwrap();
        assert_eq!(built["thinking"]["type"], json!("adaptive"), "{model}");
    }
    let manual = build(
        "claude-opus-4-6",
        &with_effort(vec![user("hi")], Effort::High),
    )
    .unwrap();
    assert_eq!(manual["thinking"]["type"], json!("enabled"));
}

#[test]
fn headers_carry_the_oauth_set_and_never_an_api_key() {
    let credential = Credential {
        bearer: "TEST-TOKEN".to_string(),
        account_id: None,
    };
    let headers = build_headers(account(), &credential, &json!({ "stream": true }));
    let expected = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "text/event-stream".to_string()),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
        (
            "user-agent".to_string(),
            format!("p1/{}", env!("CARGO_PKG_VERSION")),
        ),
        ("authorization".to_string(), "Bearer TEST-TOKEN".to_string()),
        (
            "anthropic-dangerous-direct-browser-access".to_string(),
            "true".to_string(),
        ),
        ("x-app".to_string(), "cli".to_string()),
        (
            "anthropic-beta".to_string(),
            "oauth-2025-04-20,claude-code-20250219".to_string(),
        ),
    ];
    assert_eq!(headers, expected);
    assert!(
        !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
    );
}

#[test]
fn interleaved_thinking_beta_is_present_only_for_manual_budget_thinking() {
    let credential = Credential {
        bearer: "T".to_string(),
        account_id: None,
    };
    let manual = build_headers(
        account(),
        &credential,
        &json!({ "thinking": { "type": "enabled", "budget_tokens": 4_096 } }),
    );
    let beta = manual
        .iter()
        .find(|(name, _)| name == "anthropic-beta")
        .unwrap()
        .1
        .clone();
    assert_eq!(
        beta,
        "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14"
    );

    let adaptive = build_headers(
        account(),
        &credential,
        &json!({ "thinking": { "type": "adaptive", "display": "summarized" } }),
    );
    let beta = adaptive
        .iter()
        .find(|(name, _)| name == "anthropic-beta")
        .unwrap()
        .1
        .clone();
    assert_eq!(beta, "oauth-2025-04-20,claude-code-20250219");
}

#[test]
fn assistant_text_and_tool_calls_coalesce_into_one_message() {
    let history = vec![
        user("go"),
        assistant(vec![
            text_block("working"),
            tool_call("call_1", "read", "{\"path\":\"a\"}"),
            tool_call("call_2", "grep", "{\"pattern\":\"b\"}"),
        ]),
        result("call_1", ToolStatus::Ok),
        result("call_2", ToolStatus::Error),
    ];
    let built = build("claude-sonnet-4-6", &request(history)).unwrap();
    let messages = built["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "user"]);

    let blocks = messages[1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0], json!({ "type": "text", "text": "working" }));
    assert_eq!(blocks[1]["type"], json!("tool_use"));
    assert_eq!(blocks[1]["id"], json!("call_1"));
    assert_eq!(blocks[1]["name"], json!("read"));
    assert_eq!(blocks[1]["input"], json!({ "path": "a" }));
    assert_eq!(blocks[2]["id"], json!("call_2"));

    let results = messages[2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["type"], json!("tool_result"));
    assert_eq!(results[0]["is_error"], json!(false));
    assert_eq!(results[1]["is_error"], json!(true));
}

#[test]
fn user_text_after_tool_results_coalesces_into_the_same_user_message() {
    let history = vec![result("call_1", ToolStatus::Ok), user("next prompt")];
    let built = build("m", &request(history)).unwrap();
    let messages = built["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], json!("user"));
    assert_eq!(messages[0]["content"][0]["type"], json!("tool_result"));
    assert_eq!(
        messages[0]["content"][1],
        json!({ "type": "text", "text": "next prompt", "cache_control": ephemeral() })
    );
}

#[test]
fn inbox_items_are_user_role_text() {
    let history = vec![Item::Inbox {
        kind: InboxKind::Steering,
        text: "steer".to_string(),
    }];
    let built = build("m", &request(history)).unwrap();
    assert_eq!(
        built["messages"][0],
        json!({ "role": "user", "content": [
            { "type": "text", "text": "steer", "cache_control": ephemeral() }
        ] })
    );
}

#[test]
fn invalid_tool_input_is_mapped_to_an_empty_object() {
    let history = vec![
        user("go"),
        assistant(vec![tool_call("call_1", "read", "{\"path\": ")]),
    ];
    let built = build("m", &request(history)).unwrap();
    assert_eq!(built["messages"][1]["content"][0]["input"], json!({}));
}

#[test]
fn history_starting_with_an_assistant_turn_is_an_invalid_request() {
    let error = build("m", &request(vec![assistant(vec![text_block("hi")])])).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

#[test]
fn an_assistant_item_with_only_dropped_blocks_contributes_no_message() {
    let foreign = ReplayData {
        origin: Origin {
            route: "other-route".to_string(),
            model: "m".to_string(),
        },
        version: 1,
        payload: json!({ "type": "thinking", "signature": "sig" }),
    };
    let history = vec![
        assistant(vec![AssistantBlock::Reasoning {
            text: "foreign thought".to_string(),
            replay: Some(foreign),
        }]),
        user("hi"),
    ];
    let built = build("m", &request(history)).unwrap();
    let messages = built["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], json!("user"));
}

#[test]
fn replay_is_same_origin_gated_and_byte_exact() {
    let same = Origin {
        route: ROUTE.to_string(),
        model: "claude-sonnet-4-6".to_string(),
    };
    let other_route = Origin {
        route: "other-route".to_string(),
        model: "claude-sonnet-4-6".to_string(),
    };
    let other_model = Origin {
        route: ROUTE.to_string(),
        model: "claude-opus-4-6".to_string(),
    };
    let thinking = |origin: Origin, version: u32| ReplayData {
        origin,
        version,
        payload: json!({ "type": "thinking", "signature": "sig-1" }),
    };
    let history = vec![
        user("go"),
        assistant(vec![
            // Empty visible thinking still replays when signed.
            AssistantBlock::Reasoning {
                text: String::new(),
                replay: Some(thinking(same.clone(), 1)),
            },
            AssistantBlock::Reasoning {
                text: "foreign thought".to_string(),
                replay: Some(thinking(other_route, 1)),
            },
            AssistantBlock::Reasoning {
                text: "foreign thought".to_string(),
                replay: Some(thinking(other_model, 1)),
            },
            AssistantBlock::Reasoning {
                text: "stale thought".to_string(),
                replay: Some(thinking(same.clone(), 2)),
            },
            AssistantBlock::Reasoning {
                text: "no replay".to_string(),
                replay: None,
            },
            AssistantBlock::Reasoning {
                text: String::new(),
                replay: Some(ReplayData {
                    origin: same.clone(),
                    version: 1,
                    payload: json!({ "type": "redacted_thinking", "data": "opaque-redacted" }),
                }),
            },
            text_block("answer"),
        ]),
    ];
    let built = build("claude-sonnet-4-6", &request(history)).unwrap();
    let blocks = built["messages"][1]["content"].as_array().unwrap();
    assert_eq!(
        blocks,
        &vec![
            json!({ "type": "thinking", "thinking": "", "signature": "sig-1" }),
            json!({ "type": "redacted_thinking", "data": "opaque-redacted" }),
            json!({ "type": "text", "text": "answer" }),
        ]
    );
}

#[test]
fn empty_assistant_text_is_skipped() {
    let history = vec![user("go"), assistant(vec![text_block("")]), user("again")];
    let built = build("m", &request(history)).unwrap();
    let messages = built["messages"].as_array().unwrap();
    // The assistant message is dropped entirely; the two user items are now
    // adjacent and coalesce into one message.
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], json!("user"));
    let content = messages[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["text"], json!("go"));
    assert_eq!(content[1]["text"], json!("again"));
}

// ---------------------------------------------------------------------------
// describe / validate / stream setup errors
// ---------------------------------------------------------------------------

struct NoCredentials;

impl CredentialSource for NoCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "no credential",
            ))
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "no credential",
            ))
        })
    }
}

fn provider(model: &str) -> AnthropicProvider {
    let transport: Arc<dyn Transport> = Arc::new(ScriptedTransport::new(Vec::new()));
    AnthropicProvider::new(
        route(),
        model,
        Arc::new(profile(model)),
        transport,
        Arc::new(NoCredentials),
    )
    .expect("the profile is expressible on the Messages wire")
}

#[test]
fn describe_reports_the_route_facts() {
    let description = provider("claude-sonnet-4-6").describe();
    assert_eq!(description.origin.route, ROUTE);
    assert_eq!(description.origin.model, "claude-sonnet-4-6");
    assert!(!description.supports_freeform_tools);
    assert_eq!(
        description.mandatory_prompt_prefix.as_deref(),
        Some(IDENTITY)
    );
    assert!(!description.reports_cost);
}

#[test]
fn validate_rejects_a_freeform_declaration_by_name() {
    let mut request = request(vec![user("hi")]);
    request.tools = vec![ToolDeclaration {
        name: "apply_patch".to_string(),
        description: "freeform".to_string(),
        kind: DeclarationKind::Freeform {
            grammar: Some(Grammar {
                syntax: "lark".to_string(),
                definition: "start: /.*/".to_string(),
            }),
        },
    }];
    let error = provider("m").validate(&request).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("apply_patch"), "{}", error.message);
}

#[test]
fn validate_rejects_unknown_route_native_options_and_ignores_other_namespaces() {
    let mut request = request(vec![user("hi")]);
    request
        .options
        .native
        .insert("openai-codex.verbosity".to_string(), json!("low"));
    assert!(provider("m").validate(&request).is_ok());

    request
        .options
        .native
        .insert("anthropic-messages.future_flag".to_string(), json!(true));
    let error = provider("m").validate(&request).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(
        error.message.contains("anthropic-messages.future_flag"),
        "{}",
        error.message
    );
}

#[test]
fn validate_rejects_a_zero_output_cap() {
    let mut request = request(vec![user("hi")]);
    request.options.max_output_tokens = Some(0);
    let error = provider("m").validate(&request).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

#[tokio::test]
async fn stream_returns_err_only_for_unbuildable_requests() {
    let mut freeform = request(vec![user("hi")]);
    freeform.tools = vec![ToolDeclaration {
        name: "apply_patch".to_string(),
        description: "freeform".to_string(),
        kind: DeclarationKind::Freeform { grammar: None },
    }];
    let error = provider("m")
        .stream(freeform, CancellationToken::new())
        .await
        .err()
        .expect("freeform must fail before any network use");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);

    let error = provider("m")
        .stream(
            request(vec![assistant(vec![text_block("leaderless")])]),
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("an unbuildable history must fail before any network use");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

#[test]
fn with_base_url_and_with_retry_are_chainable() {
    let provider = provider("m")
        .with_base_url("https://example.test/")
        .with_retry(RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        });
    // Construction succeeded and nothing touched the network.
    assert_eq!(provider.describe().origin.model, "m");
}
