//! The `api-key` dialect (ADR-0197): the public Messages API with an Anthropic API key.
//! It keeps the prompt-cache markers and the documented betas and sends none of the
//! Claude Code subscription's identity: no identity block, no OAuth betas, no `x-app`.

use std::collections::BTreeMap;

use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{CacheKeySupport, Effort, Item, ModelOptions, ProviderRequest};
use p1_model_profile::ModelProfile;
use p1_provider_anthropic::{
    MessagesAccount, MessagesAdapterSettings, MessagesRoute, build_headers, lower_request,
    validate_composition,
};
use p1_provider_http::Credential;
use serde_json::{Value, json};

/// `routes/anthropic.toml` as the adapter sees it.
fn route(long_context: bool) -> MessagesRoute {
    MessagesRoute {
        origin_route: "anthropic-messages/anthropic".into(),
        endpoint: "https://api.anthropic.com".into(),
        account: MessagesAccount::ApiKey,
        long_context,
    }
}

fn sonnet_5() -> ModelProfile {
    ModelProfile::from_toml(
        "claude-sonnet-5",
        include_str!("../../../profiles/claude-sonnet-5.toml"),
    )
    .unwrap()
}

fn sonnet_4_6() -> ModelProfile {
    ModelProfile::from_toml(
        "claude-sonnet-4-6",
        include_str!("../../../profiles/claude-sonnet-4-6.toml"),
    )
    .unwrap()
}

fn request(effort: Option<Effort>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![ToolDeclaration {
            name: "read".into(),
            description: "the read tool".into(),
            kind: DeclarationKind::Function {
                input_schema: json!({"type": "object"}),
            },
        }],
        options: ModelOptions {
            reasoning_effort: effort,
            ..ModelOptions::default()
        },
    }
}

fn header_map(headers: Vec<(String, String)>) -> BTreeMap<String, String> {
    headers.into_iter().collect()
}

fn credential() -> Credential {
    Credential {
        bearer: "FAKE-api-key".into(),
        account_id: None,
    }
}

#[test]
fn the_api_key_lane_keeps_the_cache_markers_and_sends_no_claude_code_identity() {
    let lowered = lower_request(
        &route(false),
        "claude-sonnet-5",
        &sonnet_5(),
        &request(Some(Effort::High)),
    )
    .unwrap();
    assert_eq!(lowered.path, "/v1/messages");
    let body: Value = serde_json::from_slice(&lowered.body).unwrap();
    let ephemeral = json!({"type": "ephemeral"});
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-5",
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "high"},
            "max_tokens": 32_000,
            "stream": true,
            "system": [{"type": "text", "text": "SYS", "cache_control": ephemeral}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": ephemeral}
            ]}],
            "tools": [{
                "name": "read",
                "description": "the read tool",
                "input_schema": {"type": "object"},
                "cache_control": ephemeral,
            }],
        })
    );
    // The credential-free set: exactly the protocol headers, no beta and no client identity.
    assert_eq!(
        lowered.headers,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "text/event-stream".to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            (
                "user-agent".to_string(),
                format!("p1/{}", env!("CARGO_PKG_VERSION"))
            ),
        ]
    );
    let headers = header_map(build_headers(MessagesAccount::ApiKey, &credential(), &body));
    assert_eq!(headers["x-api-key"], "FAKE-api-key");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    for forbidden in [
        "anthropic-beta",
        "anthropic-dangerous-direct-browser-access",
        "x-app",
        "x-opencode-session",
    ] {
        assert!(!headers.contains_key(forbidden), "{forbidden}: {headers:?}");
    }
    let text = String::from_utf8(lowered.body).unwrap();
    assert!(!text.contains("Claude Code"), "{text}");
    assert!(!text.contains("oauth"), "{text}");
}

#[test]
fn the_api_key_route_describes_no_prompt_prefix_and_takes_no_cache_key() {
    let description = route(false).describe("claude-sonnet-5");
    assert_eq!(description.mandatory_prompt_prefix, None);
    assert_eq!(description.cache_key, CacheKeySupport::Unsupported);
    assert_eq!(description.origin.route, "anthropic-messages/anthropic");
}

#[test]
fn only_a_manual_budget_carries_the_documented_interleaved_thinking_beta() {
    let beta = |profile: &ModelProfile, model: &str, effort| {
        let lowered = lower_request(&route(false), model, profile, &request(effort)).unwrap();
        let body: Value = serde_json::from_slice(&lowered.body).unwrap();
        header_map(build_headers(MessagesAccount::ApiKey, &credential(), &body))
            .get("anthropic-beta")
            .cloned()
    };
    assert_eq!(
        beta(&sonnet_4_6(), "claude-sonnet-4-6", Some(Effort::High)).as_deref(),
        Some("interleaved-thinking-2025-05-14")
    );
    assert_eq!(beta(&sonnet_4_6(), "claude-sonnet-4-6", None), None);
    assert_eq!(
        beta(&sonnet_5(), "claude-sonnet-5", Some(Effort::High)),
        None
    );
}

#[test]
fn long_context_on_the_api_key_lane_sends_the_context_beta_alone_or_joined() {
    validate_composition(&route(true), "claude-sonnet-5", &sonnet_5())
        .expect("an API-key route may ask for the 1M window");
    let beta = |profile: &ModelProfile, model: &str| {
        let lowered =
            lower_request(&route(true), model, profile, &request(Some(Effort::High))).unwrap();
        header_map(lowered.headers)["anthropic-beta"].clone()
    };
    assert_eq!(
        beta(&sonnet_5(), "claude-sonnet-5"),
        "context-1m-2025-08-07"
    );
    assert_eq!(
        beta(&sonnet_4_6(), "claude-sonnet-4-6"),
        "interleaved-thinking-2025-05-14,context-1m-2025-08-07"
    );
}

#[test]
fn the_api_key_dialect_requires_the_x_api_key_credential_header() {
    let settings = |value: Value| serde_json::from_value::<MessagesAdapterSettings>(value);
    assert_eq!(
        settings(json!({"dialect": "api-key", "credential_header": "x-api-key"})).unwrap(),
        MessagesAdapterSettings {
            account: MessagesAccount::ApiKey,
            long_context: false,
        }
    );
    let error = settings(json!({"dialect": "api-key"})).unwrap_err();
    assert!(error.to_string().contains("credential_header"), "{error}");
}
