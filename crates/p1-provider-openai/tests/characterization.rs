//! Characterization tests for the OpenAI Responses (Codex subscription) adapter.
//!
//! These pin TODAY's model/effort/route policy so the ADR-0039 steps 3–4
//! refactor can be proven behaviour-preserving on the wire. Current behaviour is
//! the oracle: a failing assertion here means the wire output changed, not that
//! this test is wrong.
//!
//! Existing tests already pin the pieces this file does not repeat:
//! `src/request.rs::default_body_is_exact` (`store`/`stream`/`instructions`
//! placement), `src/request.rs::builds_the_exact_header_set` (every static
//! header), `src/request.rs::cache_key_headers_use_the_clamped_value`,
//! `src/request.rs::reasoning_effort_adds_reasoning_and_include`,
//! `src/request.rs::reasoning_replay_requires_this_origin_and_version`,
//! `src/parser.rs::completed_keeps_the_configured_model_as_origin`,
//! `tests/fixtures_drive.rs::headers_and_request_body_are_sent_exactly` and
//! `tests/fixtures_drive.rs::reasoning_turn_round_trips_replay`.

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::history::{AssistantBlock, AssistantItem, Item, Origin, ReplayData};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, ModelOptions, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest, RouteDescription,
};
use p1_provider_http::testing::{ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, Transport};
use p1_provider_openai::{OpenAiCodexProvider, ROUTE, build_headers, build_request};
use serde_json::{Value, json};

mod fixtures;

const CASES: &str = include_str!("fixtures/characterization/request_cases.json");

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_string(),
    }
}

fn reasoning_block(route: &str, model: &str) -> Item {
    Item::Assistant(AssistantItem {
        origin: Origin {
            route: ROUTE.to_string(),
            model: "gpt-5.6-sol".to_string(),
        },
        blocks: vec![AssistantBlock::Reasoning {
            text: "hidden chain".to_string(),
            replay: Some(ReplayData {
                origin: Origin {
                    route: route.to_string(),
                    model: model.to_string(),
                },
                version: 1,
                payload: json!({ "type": "reasoning", "encrypted_content": "enc-1" }),
            }),
        }],
    })
}

/// Reconstruct the request behind each named fixture case. The fixture stores
/// only the expected output; this function is the input side.
fn case_request(name: &str) -> (&'static str, ModelOptions, Vec<Item>) {
    let mut options = ModelOptions::default();
    let mut history = vec![user("hi")];
    let model = match name {
        "default_sol" => "gpt-5.6-sol",
        "low_sol" => {
            options.reasoning_effort = Some(Effort::Low);
            "gpt-5.6-sol"
        }
        "medium_sol" => {
            options.reasoning_effort = Some(Effort::Medium);
            "gpt-5.6-sol"
        }
        "high_sol" => {
            options.reasoning_effort = Some(Effort::High);
            "gpt-5.6-sol"
        }
        "default_mini" => "gpt-5.6-sol-mini",
        "medium_mini" => {
            options.reasoning_effort = Some(Effort::Medium);
            "gpt-5.6-sol-mini"
        }
        "verbosity_medium" => {
            options
                .native
                .insert("openai-responses.verbosity".to_string(), json!("medium"));
            "gpt-5.6-sol"
        }
        "cache_key_long" => {
            options.cache_key = Some("å".repeat(70));
            "gpt-5.6-sol"
        }
        "replay_same_origin_high" => {
            options.reasoning_effort = Some(Effort::High);
            history = vec![
                user("go"),
                reasoning_block(ROUTE, "gpt-5.6-sol"),
                user("continue"),
            ];
            "gpt-5.6-sol"
        }
        "replay_foreign_route" => {
            history = vec![
                user("go"),
                reasoning_block("other-route", "gpt-5.6-sol"),
                user("continue"),
            ];
            "gpt-5.6-sol"
        }
        "replay_foreign_model" => {
            history = vec![
                user("go"),
                reasoning_block(ROUTE, "other-model"),
                user("continue"),
            ];
            "gpt-5.6-sol"
        }
        other => panic!("unknown fixture case: {other}"),
    };
    (model, options, history)
}

fn test_credential() -> Credential {
    Credential {
        bearer: "TEST-TOKEN".to_string(),
        account_id: Some("acct_1".to_string()),
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn request_for(options: ModelOptions, history: Vec<Item>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".to_string(),
        history,
        tools: Vec::new(),
        options,
    }
}

/// The full case matrix: whole body equality plus the cache-key identity
/// headers. Pins that model name changes ONLY the `model` field, that there is
/// no per-model default effort, that `reasoning`/`include` appear exactly for
/// `low|medium|high`, that `store`/`stream`/`instructions`/`text` sit in the
/// same places, and that the clamped cache key is sent as BOTH
/// `prompt_cache_key` and the `session_id`/`conversation_id` headers.
#[test]
fn request_cases_pin_the_exact_body_and_cache_headers() {
    let fixture: Value = serde_json::from_str(CASES).expect("fixture is valid JSON");
    let cases = fixture["cases"].as_array().expect("fixture has cases");
    assert_eq!(cases.len(), 11);

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let (model, options, history) = case_request(name);
        let body = build_request(model, &request_for(options, history)).unwrap();
        assert_eq!(body, case["body"], "body: {name}");

        let sent_cache_key = case["sent_cache_key"].as_str();
        let headers = build_headers(&test_credential(), sent_cache_key).unwrap();
        assert_eq!(
            header(&headers, "session_id"),
            case["session_id"].as_str(),
            "session_id: {name}"
        );
        assert_eq!(
            header(&headers, "conversation_id"),
            case["conversation_id"].as_str(),
            "conversation_id: {name}"
        );
        if let Some(key) = sent_cache_key {
            assert_eq!(
                body["prompt_cache_key"].as_str(),
                Some(key),
                "prompt_cache_key: {name}"
            );
            assert_eq!(header(&headers, "session_id"), Some(key), "{name}");
            assert_eq!(header(&headers, "conversation_id"), Some(key), "{name}");
            assert_eq!(key.chars().count(), 64, "clamped to 64 chars: {name}");
        } else {
            assert!(body.get("prompt_cache_key").is_none(), "{name}");
            assert!(header(&headers, "session_id").is_none(), "{name}");
            assert!(header(&headers, "conversation_id").is_none(), "{name}");
        }
    }
}

/// The adapter has no model-specific request policy: two model names with the
/// same options produce identical bodies except for the `model` field.
#[test]
fn model_name_changes_only_the_model_field() {
    let options = ModelOptions {
        reasoning_effort: Some(Effort::Medium),
        ..ModelOptions::default()
    };
    let sol = build_request(
        "gpt-5.6-sol",
        &request_for(options.clone(), vec![user("hi")]),
    )
    .unwrap();
    let mini = build_request("gpt-5.6-sol-mini", &request_for(options, vec![user("hi")])).unwrap();

    assert_eq!(sol["model"], json!("gpt-5.6-sol"));
    assert_eq!(mini["model"], json!("gpt-5.6-sol-mini"));
    let mut stripped_sol = sol.clone();
    let mut stripped_mini = mini.clone();
    stripped_sol.as_object_mut().unwrap().remove("model");
    stripped_mini.as_object_mut().unwrap().remove("model");
    assert_eq!(stripped_sol, stripped_mini);
}

/// `extra_high` and `max` are rejected route-wide, for every model name, not
/// silently mapped or clamped.
#[test]
fn extra_high_and_max_are_rejected_for_every_model() {
    for model in ["gpt-5.6-sol", "gpt-5.6-sol-mini", "gpt-test"] {
        for effort in [Effort::ExtraHigh, Effort::Max] {
            let options = ModelOptions {
                reasoning_effort: Some(effort),
                ..ModelOptions::default()
            };
            let error = build_request(model, &request_for(options, vec![user("hi")])).unwrap_err();
            assert_eq!(
                error.kind,
                ProviderErrorKind::InvalidRequest,
                "{model} {effort:?}"
            );
        }
    }
}

/// An explicit EMPTY cache key is treated as a key, not as "no caching": the
/// body gets `prompt_cache_key: ""` and the wire gets empty `session_id` /
/// `conversation_id` headers. SUSPECTED DEFECT: an empty key is preserved rather
/// than normalised to `None`; whether the route accepts it is unverified. Pinned
/// so a future normalisation is a visible, deliberate change.
#[test]
fn empty_cache_key_is_sent_as_an_empty_key_not_absent() {
    let options = ModelOptions {
        cache_key: Some(String::new()),
        ..ModelOptions::default()
    };
    let body = build_request("gpt-5.6-sol", &request_for(options, vec![user("hi")])).unwrap();
    assert_eq!(body["prompt_cache_key"], json!(""));
    let headers = build_headers(&test_credential(), Some("")).unwrap();
    assert_eq!(header(&headers, "session_id"), Some(""));
    assert_eq!(header(&headers, "conversation_id"), Some(""));
}

/// The cache-key identity headers are appended after the static set, in this
/// order, and the whole header list is byte-exact. (The no-cache static list is
/// pinned by `src/request.rs::builds_the_exact_header_set`.)
#[test]
fn cache_key_header_list_is_appended_in_order() {
    let headers = build_headers(
        &test_credential(),
        Some("0123456789012345678901234567890123456789012345678901234567890123"),
    )
    .unwrap();
    assert_eq!(
        headers,
        vec![
            ("Authorization".to_string(), "Bearer TEST-TOKEN".to_string()),
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
            (
                "session_id".to_string(),
                "0123456789012345678901234567890123456789012345678901234567890123".to_string()
            ),
            (
                "conversation_id".to_string(),
                "0123456789012345678901234567890123456789012345678901234567890123".to_string()
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// provider.stream cache-key wiring
// ---------------------------------------------------------------------------

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Ok(Credential {
                bearer: "TEST-TOKEN".to_string(),
                account_id: Some("acct_1".to_string()),
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
                account_id: Some("acct_1".to_string()),
            })
        })
    }
}

/// The provider passes the SAME clamped cache key to the body's
/// `prompt_cache_key` and to the `session_id`/`conversation_id` headers. The
/// named fixture cases pin `build_request`/`build_headers`; this pins the
/// `OpenAiCodexProvider::stream` wiring between them.
#[tokio::test]
async fn provider_stream_sends_the_clamped_cache_key_in_the_body_and_headers() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let provider = OpenAiCodexProvider::new(
        "gpt-5.6-sol",
        Arc::new(transport.clone()),
        Arc::new(FixedCredentials),
    );
    let options = ModelOptions {
        cache_key: Some("å".repeat(70)),
        ..ModelOptions::default()
    };
    let mut stream = provider
        .stream(
            ProviderRequest {
                system_prompt: "SYS".to_string(),
                history: vec![user("hi")],
                tools: Vec::new(),
                options,
            },
            CancellationToken::new(),
        )
        .await
        .expect("setup succeeds");
    while stream.next().await.is_some() {}

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let key = body["prompt_cache_key"].as_str().expect("body cache key");
    assert_eq!(key.chars().count(), 64);
    let headers = &requests[0].headers;
    assert_eq!(header(headers, "session_id"), Some(key));
    assert_eq!(header(headers, "conversation_id"), Some(key));
}

// ---------------------------------------------------------------------------
// describe()
// ---------------------------------------------------------------------------

fn provider(model: &str) -> OpenAiCodexProvider {
    let transport: Arc<dyn Transport> = Arc::new(ScriptedTransport::new(Vec::new()));
    OpenAiCodexProvider::new(model, transport, Arc::new(FixedCredentials))
}

/// Every shipped model name in the environments that use this adapter.
fn shipped_models() -> Vec<&'static str> {
    const ENVIRONMENTS: &[&str] = &[include_str!("../../../environments/gpt/environment.toml")];
    ENVIRONMENTS
        .iter()
        .map(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("model")?.split('"').nth(1))
                .expect("a shipped environment names a model")
        })
        .collect()
}

#[test]
fn describe_matches_every_shipped_model() {
    let models = shipped_models();
    assert_eq!(models, vec!["gpt-5.6-sol"]);
    for model in models {
        let expected = RouteDescription {
            origin: Origin {
                route: ROUTE.to_string(),
                model: model.to_string(),
            },
            supports_freeform_tools: true,
            mandatory_prompt_prefix: None,
            reports_cost: false,
        };
        assert_eq!(provider(model).describe(), expected, "{model}");
    }
}
