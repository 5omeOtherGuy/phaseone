//! Characterization tests for the Anthropic Messages adapter.
//!
//! These pin TODAY's model/effort/route policy so the ADR-0039 steps 3–4
//! refactor (moving model classification, thinking budgets and route identity
//! into `p1-model-profile` and route data) can be proven behaviour-preserving on
//! the wire. Current behaviour is the oracle: a failing assertion here means the
//! wire output changed, not that this test is wrong.
//!
//! Existing tests already pin the pieces this file does not repeat:
//! `tests/request.rs::golden_request_whole_body`,
//! `tests/request.rs::manual_thinking_budgets_and_the_output_margin`,
//! `tests/request.rs::an_explicit_output_cap_is_raised_to_keep_budget_below_max_tokens`,
//! `tests/request.rs::interleaved_thinking_beta_is_present_only_for_manual_budget_thinking`,
//! `tests/request.rs::headers_carry_the_oauth_set_and_never_an_api_key`,
//! `tests/request.rs::replay_is_same_origin_gated_and_byte_exact`,
//! `tests/request.rs::describe_reports_the_route_facts`.
//! The shared `tests/conformance.rs::conformance` run pins that a reasoning
//! block replays on `claude-sonnet-5` while the fixture echoes a dated alias
//! (`claude-sonnet-4-6`) — i.e. origin is the configured model, not the echo.

#[allow(dead_code)]
mod fixtures;

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::history::{AssistantBlock, AssistantItem, Item, Origin, ReplayData};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, ModelOptions, Outcome, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, RouteDescription, StreamEvent,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{
    AnthropicProvider, MessagesAccount, MessagesRoute, ROUTE, build_headers, build_request,
};
use p1_provider_http::testing::{ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, Transport};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const MATRIX: &str = include_str!("fixtures/characterization/model_effort_matrix.json");

/// The route data these expectations were recorded with: today's origin route and
/// endpoint. ADR-0039 step 4 moved them into `routes/anthropic-subscription.toml`
/// without changing a byte of what the adapter sends.
fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
    }
}

/// The account these expectations were recorded with.
fn account() -> MessagesAccount {
    MessagesAccount::ClaudeCodeSubscription
}

/// The profile the OLD rule selected for `model`: a name starting with one of
/// `claude-fable-5`, `claude-opus-5` or `claude-sonnet-5` was the effort-level lane
/// (all five efforts); every other name — the budget models AND the near misses
/// (`claude-opus5`, `claude-fable-4`) — took the manual budget table. This mapping
/// documents what the explicit `profiles/claude-*.toml` records replaced; the
/// fixture's expected values are untouched.
fn profile_for(model: &str) -> ModelProfile {
    let effort_level = ["claude-fable-5", "claude-opus-5", "claude-sonnet-5"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
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
        efforts: vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::ExtraHigh,
            Effort::Max,
        ],
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

/// `build_request` over the route and the profile `model` selects.
fn build(model: &str, request: &ProviderRequest) -> Result<Value, ProviderError> {
    build_request(&route(), model, &profile_for(model), request)
}

/// The adapter composed with the route and the profile `model` selects.
fn provider_with(
    model: &str,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
) -> AnthropicProvider {
    AnthropicProvider::new(
        route(),
        model,
        Arc::new(profile_for(model)),
        transport,
        credentials,
    )
    .expect("the profile is expressible on the Messages wire")
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".to_string(),
        history: vec![Item::User {
            text: "hi".to_string(),
        }],
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

fn test_credential() -> Credential {
    Credential {
        bearer: "TEST-TOKEN".to_string(),
        account_id: None,
    }
}

fn effort_from_name(name: Option<&str>) -> Option<Effort> {
    match name {
        None => None,
        Some("low") => Some(Effort::Low),
        Some("medium") => Some(Effort::Medium),
        Some("high") => Some(Effort::High),
        Some("extra_high") => Some(Effort::ExtraHigh),
        Some("max") => Some(Effort::Max),
        Some(other) => panic!("unknown effort in fixture: {other}"),
    }
}

fn beta_header(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(name, _)| name == "anthropic-beta")
        .map(|(_, value)| value.clone())
        .expect("every request carries anthropic-beta")
}

/// The full {model class} × {no effort, every `Effort`} matrix. This pins the
/// exact `thinking`, `output_config`, `max_tokens` and `anthropic-beta` values
/// for every adaptive prefix, a dated adaptive alias, a prefix near-miss that is
/// nevertheless adaptive (`claude-opus-50`), a non-adaptive model and three
/// non-adaptive near-misses.
#[test]
fn model_effort_matrix_pins_thinking_output_config_max_tokens_and_beta() {
    let fixture: Value = serde_json::from_str(MATRIX).expect("fixture is valid JSON");
    let cases = fixture["cases"].as_array().expect("fixture has cases");
    assert_eq!(cases.len(), 9 * 6, "9 models × 6 effort states");

    for case in cases {
        let model = case["model"].as_str().unwrap();
        let effort = effort_from_name(case["effort"].as_str());
        let mut req = request();
        req.options.reasoning_effort = effort;
        let body = build(model, &req).unwrap();

        let actual_output_config = body.get("output_config").cloned().unwrap_or(Value::Null);
        assert_eq!(
            body["thinking"], case["thinking"],
            "thinking: {model} {effort:?}"
        );
        assert_eq!(
            actual_output_config, case["output_config"],
            "output_config: {model} {effort:?}"
        );
        assert_eq!(
            body["max_tokens"], case["max_tokens"],
            "max_tokens: {model} {effort:?}"
        );
        assert_eq!(
            Value::String(beta_header(&build_headers(
                account(),
                &test_credential(),
                &body
            ))),
            case["beta"],
            "anthropic-beta: {model} {effort:?}"
        );
    }
}

/// An EXPLICIT output cap that the manual thinking budget meets or exceeds is
/// rejected — with cap, budget and the minimum workable cap — while every other
/// explicit/derived cap keeps its pinned value.
///
/// NOTE: ADR-0039 replaces the adapter's silent raise of an explicit output cap
/// with this rejection.
/// One matrix row: `(model, effort, cap, thinking, max_tokens)`. `max_tokens` is
/// `None` exactly for the caps that were silently RAISED before and are rejected
/// now; the thinking value still names the budget the rejection message cites.
type CapCase = (&'static str, Option<Effort>, u32, Value, Option<u64>);

#[test]
fn explicit_output_cap_below_the_thinking_budget_is_rejected() {
    let cases: Vec<CapCase> = vec![
        // Manual budget: the cap is raised to budget + 8192 only when the
        // budget would otherwise meet or exceed it.
        (
            "claude-opus-4-6",
            Some(Effort::Low),
            1_000,
            json!({ "type": "enabled", "budget_tokens": 4_096 }),
            None,
        ),
        (
            "claude-opus-4-6",
            Some(Effort::Low),
            4_096,
            json!({ "type": "enabled", "budget_tokens": 4_096 }),
            None,
        ),
        (
            "claude-opus-4-6",
            Some(Effort::Low),
            4_097,
            json!({ "type": "enabled", "budget_tokens": 4_096 }),
            Some(4_097),
        ),
        (
            "claude-opus-4-6",
            Some(Effort::Low),
            10_000,
            json!({ "type": "enabled", "budget_tokens": 4_096 }),
            Some(10_000),
        ),
        (
            "claude-opus-4-6",
            Some(Effort::High),
            20_480,
            json!({ "type": "enabled", "budget_tokens": 20_480 }),
            None,
        ),
        (
            "claude-opus-4-6",
            Some(Effort::ExtraHigh),
            32_768,
            json!({ "type": "enabled", "budget_tokens": 32_768 }),
            None,
        ),
        (
            "claude-opus-4-6",
            Some(Effort::ExtraHigh),
            40_000,
            json!({ "type": "enabled", "budget_tokens": 32_768 }),
            Some(40_000),
        ),
        // No thinking: the cap is used verbatim.
        ("claude-opus-4-6", None, 4_242, Value::Null, Some(4_242)),
        // Adaptive thinking never raises the cap.
        (
            "claude-sonnet-5",
            Some(Effort::High),
            1_234,
            json!({ "type": "adaptive", "display": "summarized" }),
            Some(1_234),
        ),
        ("claude-sonnet-5", None, 1_234, Value::Null, Some(1_234)),
    ];

    for (model, effort, cap, expected_thinking, expected_max) in cases {
        let mut req = request();
        req.options.reasoning_effort = effort;
        req.options.max_output_tokens = Some(cap);
        match expected_max {
            // A cap at or below the manual budget is rejected, with the cap,
            // the budget and the smallest cap that would work in the message.
            None => {
                let error = build(model, &req).unwrap_err();
                assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
                let budget = expected_thinking["budget_tokens"].as_u64().unwrap();
                for part in [
                    format!("max_output_tokens {cap}"),
                    format!("thinking budget {budget}"),
                    format!("is {}", budget + 1),
                ] {
                    assert!(
                        error.message.contains(&part),
                        "{model} {effort:?} cap {cap}: {} (wanted `{part}`)",
                        error
                    );
                }
            }
            Some(expected_max) => {
                let body = build(model, &req).unwrap();
                let actual_thinking = body.get("thinking").cloned().unwrap_or(Value::Null);
                assert_eq!(
                    actual_thinking, expected_thinking,
                    "{model} {effort:?} cap {cap}"
                );
                assert_eq!(
                    body["max_tokens"],
                    json!(expected_max),
                    "{model} {effort:?} cap {cap}"
                );
            }
        }
    }
}

/// The shipped environment's model (`environments/claude/environment.toml` and
/// `claude-delegating`) resolves to the adaptive lane, so the whole body of a
/// medium-effort request is pinned here. This is the request the shipped
/// environment actually produces.
#[test]
fn golden_shipped_model_medium_request_whole_body() {
    let mut req = request();
    req.options.reasoning_effort = Some(Effort::Medium);
    let body = build("claude-sonnet-5", &req).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-5",
            "thinking": { "type": "adaptive", "display": "summarized" },
            "output_config": { "effort": "medium" },
            "max_tokens": 32_000,
            "stream": true,
            "system": [
                { "type": "text", "text": IDENTITY },
                { "type": "text", "text": "SYS", "cache_control": { "type": "ephemeral" } },
            ],
            "messages": [{
                "role": "user",
                "content": [{ "type": "text", "text": "hi", "cache_control": { "type": "ephemeral" } }],
            }],
        })
    );
}

/// A same-origin replay on an adaptive model is byte-exact AND coexists with the
/// adaptive `thinking` / `output_config` pair. The existing manual-model replay
/// test does not cover the adaptive class.
#[test]
fn replay_on_an_adaptive_model_coexists_with_adaptive_thinking() {
    let replay = ReplayData {
        origin: Origin {
            route: ROUTE.to_string(),
            model: "claude-sonnet-5".to_string(),
        },
        version: 1,
        payload: json!({ "type": "thinking", "signature": "sig-adaptive" }),
    };
    let mut req = request();
    req.options.reasoning_effort = Some(Effort::Medium);
    req.history = vec![
        Item::User {
            text: "go".to_string(),
        },
        Item::Assistant(AssistantItem {
            origin: Origin {
                route: ROUTE.to_string(),
                model: "claude-sonnet-5".to_string(),
            },
            blocks: vec![AssistantBlock::Reasoning {
                text: "thought".to_string(),
                replay: Some(replay),
            }],
        }),
        Item::User {
            text: "continue".to_string(),
        },
    ];
    let body = build("claude-sonnet-5", &req).unwrap();
    assert_eq!(
        body["thinking"],
        json!({ "type": "adaptive", "display": "summarized" })
    );
    assert_eq!(body["output_config"], json!({ "effort": "medium" }));
    assert_eq!(
        body["messages"][1]["content"],
        json!([{ "type": "thinking", "thinking": "thought", "signature": "sig-adaptive" }])
    );
}

// ---------------------------------------------------------------------------
// parser origin identity (replay-relevant model name)
// ---------------------------------------------------------------------------

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Ok(Credential {
                bearer: "TEST-TOKEN".to_string(),
                account_id: None,
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Ok(Credential {
                bearer: "TEST-TOKEN".to_string(),
                account_id: None,
            })
        })
    }
}

/// The parser tags the completed item with the CONFIGURED model, never the
/// model name the response echoes. The fixture's `message_start` echoes
/// `claude-sonnet-4-6` while the provider is configured `claude-sonnet-5`, so
/// this pins that a same-origin replay stays valid on the next request and that
/// the echoed alias would be treated as foreign.
#[tokio::test]
async fn parser_origin_is_the_configured_model_not_the_echoed_alias() {
    let transport =
        ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::reasoning_turn)]);
    let provider = provider_with(
        "claude-sonnet-5",
        Arc::new(transport) as Arc<dyn Transport>,
        Arc::new(FixedCredentials),
    );
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("a valid request starts a stream");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let completed = match events.last() {
        Some(StreamEvent::Finished(Outcome::Completed(completed))) => completed.clone(),
        other => panic!("expected a completion, got {other:?}"),
    };
    assert_eq!(completed.item.origin.model, "claude-sonnet-5");
    assert!(
        fixtures::reasoning_turn.contains("claude-sonnet-4-6"),
        "the fixture must echo a different (dated alias) model"
    );

    let follow_up = ProviderRequest {
        system_prompt: "SYS".to_string(),
        history: vec![
            Item::User {
                text: "next".to_string(),
            },
            Item::Assistant(completed.item.clone()),
        ],
        tools: Vec::new(),
        options: ModelOptions::default(),
    };
    let configured = build("claude-sonnet-5", &follow_up).unwrap();
    assert_eq!(
        configured["messages"][1]["content"],
        json!([
            { "type": "thinking", "thinking": "Let me think", "signature": "sig-abc" },
            { "type": "text", "text": "Answer" },
        ])
    );
    let echoed = build("claude-sonnet-4-6", &follow_up).unwrap();
    assert!(
        !echoed["messages"][1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["type"] == json!("thinking")),
        "a replay recorded for claude-sonnet-5 is foreign to claude-sonnet-4-6"
    );
}

// ---------------------------------------------------------------------------
// describe()
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
    provider_with(model, transport, Arc::new(NoCredentials))
}

/// Every shipped model name in the environments that use this adapter: the profile
/// each environment selects, bound to its wire model by the shipped route file.
/// ADR-0039 step 4 replaced the environments' `model` key with `route` + `profile`.
fn shipped_models() -> Vec<&'static str> {
    const ENVIRONMENTS: &[&str] = &[
        include_str!("../../../environments/claude/environment.toml"),
        include_str!("../../../environments/claude-delegating/environment.toml"),
    ];
    const SHIPPED_ROUTE: &str = include_str!("../../../routes/anthropic-subscription.toml");
    ENVIRONMENTS
        .iter()
        .map(|text| {
            let profile = text
                .lines()
                .find_map(|line| line.strip_prefix("profile")?.split('"').nth(1))
                .expect("a shipped environment names a profile");
            SHIPPED_ROUTE
                .lines()
                .skip_while(|line| !line.trim().starts_with(&format!("[models.\"{profile}\"]")))
                .find_map(|line| line.strip_prefix("wire_model")?.split('"').nth(1))
                .expect("the shipped route binds the profile it serves")
        })
        .collect()
}

#[test]
fn describe_matches_every_shipped_model() {
    let models = shipped_models();
    assert_eq!(models, vec!["claude-sonnet-5", "claude-sonnet-5"]);
    for model in models {
        let expected = RouteDescription {
            origin: Origin {
                route: ROUTE.to_string(),
                model: model.to_string(),
            },
            supports_freeform_tools: false,
            mandatory_prompt_prefix: Some(IDENTITY.to_string()),
            reports_cost: false,
            cache_key: p1_contracts::CacheKeySupport::Unsupported,
        };
        assert_eq!(provider(model).describe(), expected, "{model}");
    }
}
