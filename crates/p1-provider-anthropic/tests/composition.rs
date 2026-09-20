//! Composition acceptance for the Messages adapter (ADR-0039 step 4, spec §7.1/§7.3):
//! the model policy lives in the profile, one lowering function decides for the
//! constructor, `validate` and the request builder, and no model NAME is consulted.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use p1_contracts::{
    BoxFuture, Effort, Item, ModelOptions, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{AnthropicProvider, MessagesAccount, MessagesRoute, build_request};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};

struct NoCredentials;

impl CredentialSource for NoCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
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
        Box::pin(async {
            Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "no credential",
            ))
        })
    }
}

fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: p1_provider_anthropic::ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
    }
}

fn budget_profile() -> ModelProfile {
    ModelProfile {
        id: "claude-opus-4-6".to_string(),
        revision: 1,
        model_id: "claude-opus-4-6".to_string(),
        family: "claude".to_string(),
        thinking: ThinkingPolicy::Budget,
        efforts: vec![Effort::Low, Effort::High],
        default_effort: None,
        thinking_budgets: [(Effort::Low, 4_096), (Effort::High, 20_480)]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
        context_tokens: None,
        max_output_tokens: None,
    }
}

fn request(options: ModelOptions) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".to_string(),
        history: vec![Item::User {
            text: "hi".to_string(),
        }],
        tools: Vec::new(),
        options,
    }
}

fn provider(profile: ModelProfile) -> Result<AnthropicProvider, ProviderError> {
    AnthropicProvider::new(
        route(),
        "claude-opus-4-6",
        Arc::new(profile),
        Arc::new(ScriptedTransport::new(Vec::new())),
        Arc::new(NoCredentials),
    )
}

/// One lowering function (spec §7.3): the constructor, `validate` and the request
/// builder agree on every refusal, message included.
#[test]
fn the_constructor_validate_and_the_builder_share_one_lowering() {
    // A policy the Messages wire cannot express: refused at construction, by the
    // builder, and by `validate` with the same message.
    for policy in [ThinkingPolicy::Enabled, ThinkingPolicy::Preserved] {
        let profile = ModelProfile {
            thinking: policy,
            thinking_budgets: BTreeMap::new(),
            ..budget_profile()
        };
        let error = provider(profile.clone()).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(
            build_request(
                &route(),
                "claude-opus-4-6",
                &profile,
                &request(ModelOptions::default())
            )
            .unwrap_err(),
            error
        );
    }

    // A cap the thinking budget meets: the same message from `validate` and the
    // builder.
    let profile = budget_profile();
    let composed = provider(profile.clone()).expect("a budget profile composes");
    let options = ModelOptions {
        reasoning_effort: Some(Effort::Low),
        max_output_tokens: Some(4_096),
        ..ModelOptions::default()
    };
    let from_validate = composed.validate(&request(options.clone())).unwrap_err();
    let from_builder =
        build_request(&route(), "claude-opus-4-6", &profile, &request(options)).unwrap_err();
    assert_eq!(from_validate, from_builder);
    assert_eq!(from_validate.kind, ProviderErrorKind::InvalidRequest);
    for part in ["max_output_tokens 4096", "thinking budget 4096", "is 4097"] {
        assert!(from_validate.message.contains(part), "{from_validate}");
    }

    // An effort the profile does not list: the same message from both.
    let options = ModelOptions {
        reasoning_effort: Some(Effort::Max),
        ..ModelOptions::default()
    };
    let profile = budget_profile();
    assert_eq!(
        provider(profile.clone())
            .expect("the profile composes")
            .validate(&request(options.clone()))
            .unwrap_err(),
        build_request(&route(), "claude-opus-4-6", &profile, &request(options)).unwrap_err()
    );

    // Nothing in the model policy depends on the wire model name: two names, one body.
    let profile = budget_profile();
    let options = ModelOptions {
        reasoning_effort: Some(Effort::High),
        ..ModelOptions::default()
    };
    let mut first = build_request(
        &route(),
        "claude-opus-4-6",
        &profile,
        &request(options.clone()),
    )
    .expect("the request builds");
    let second = build_request(&route(), "vendor/alias", &profile, &request(options))
        .expect("the same profile on another wire name builds");
    assert_eq!(first["thinking"], second["thinking"]);
    assert_eq!(first["max_tokens"], second["max_tokens"]);
    first["model"] = second["model"].clone();
    assert_eq!(first, second, "the wire name is the only difference");
}

/// The adapter decides nothing by model NAME (spec §7.1): no adaptive prefix rule, no
/// budget table and no model id literal survives in its PRODUCTION source. The unit
/// tests at the end of each file (`#[cfg(test)]`) may name models.
#[test]
fn no_model_name_decision_survives_in_the_adapter() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = std::fs::read_dir(&src)
        .expect("the adapter source is readable")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .collect::<Vec<_>>();
    files.sort();
    assert!(files.len() >= 4, "the adapter source was found: {files:?}");

    let gone = [
        "fn is_adaptive",
        "fn manual_budget",
        "ADAPTIVE_PREFIXES",
        "adaptive_prefixes",
    ];
    let model_literals = [
        "claude-opus",
        "claude-sonnet",
        "claude-fable",
        "claude-haiku",
        "gpt-",
        "deepseek",
        "glm-",
    ];
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Everything from the file's own test module on is test code.
        let production = text
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default()
            .to_string();
        for gone in gone {
            assert!(
                !production.contains(gone),
                "{name} still classifies a model by name ({gone})"
            );
        }
        for literal in model_literals {
            assert!(
                !production.contains(literal),
                "{name} still compiles a model name ({literal:?})"
            );
        }
    }
}

/// The profile is the only source of the thinking policy: the same wire model lowers
/// differently for an effort-level and a budget profile.
#[test]
fn the_profile_selects_the_lane_not_the_model_name() {
    let wire_model = "claude-opus-5";
    let base = budget_profile();
    let budget = base.clone();
    let effort_level = ModelProfile {
        thinking: ThinkingPolicy::EffortLevel,
        efforts: vec![Effort::High],
        thinking_budgets: BTreeMap::new(),
        ..base
    };
    let options = ModelOptions {
        reasoning_effort: Some(Effort::High),
        ..ModelOptions::default()
    };
    let budget_body = build_request(&route(), wire_model, &budget, &request(options.clone()))
        .expect("the budget lane builds");
    let effort_body = build_request(
        &route(),
        wire_model,
        &effort_level,
        &request(options.clone()),
    )
    .expect("the effort-level lane builds");
    assert_eq!(
        budget_body["thinking"]["type"],
        serde_json::json!("enabled"),
        "{budget_body}"
    );
    assert_eq!(
        effort_body["thinking"]["type"],
        serde_json::json!("adaptive"),
        "{effort_body}"
    );
    assert_eq!(
        effort_body["output_config"],
        serde_json::json!({ "effort": "high" })
    );
    assert!(budget_body.get("output_config").is_none());

    // The effort-level profile does not list `max`: rejected, not coerced.
    let max = ModelOptions {
        reasoning_effort: Some(Effort::Max),
        ..ModelOptions::default()
    };
    let error = build_request(&route(), wire_model, &effort_level, &request(max.clone()))
        .expect_err("an unlisted effort is rejected");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(
        error,
        provider(effort_level.clone())
            .expect("the effort-level profile composes")
            .validate(&request(max))
            .unwrap_err()
    );
}
