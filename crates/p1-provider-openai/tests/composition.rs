//! Composition acceptance for the Responses adapter (ADR-0039 step 4, spec
//! §7.1/§7.3): the model policy lives in the profile, one lowering function decides
//! for the constructor, `validate` and the request builder, and no model NAME is
//! consulted.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use p1_contracts::{
    BoxFuture, Effort, Item, ModelOptions, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::{OpenAiCodexProvider, ResponsesAccount, ResponsesRoute, build_request};

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

fn route() -> ResponsesRoute {
    ResponsesRoute {
        origin_route: p1_provider_openai::ROUTE.to_string(),
        endpoint: "https://chatgpt.com/backend-api".to_string(),
        account: ResponsesAccount::CodexSubscription,
        transport: p1_provider_openai::ResponsesTransport::Sse,
    }
}

/// A `budget` profile: composes on the Messages wire, refused here.
fn budget_profile() -> ModelProfile {
    ModelProfile {
        id: "gpt-5.6-sol".to_string(),
        revision: 1,
        model_id: "gpt-5.6-sol".to_string(),
        family: "gpt".to_string(),
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

fn provider(profile: ModelProfile) -> Result<OpenAiCodexProvider, ProviderError> {
    OpenAiCodexProvider::new(
        route(),
        "gpt-5.6-sol",
        Arc::new(profile),
        Arc::new(ScriptedTransport::new(Vec::new())),
        Arc::new(NoCredentials),
    )
}

/// One lowering function (spec §7.3): the constructor, `validate` and the request
/// builder agree on every refusal, message included.
#[test]
fn the_constructor_validate_and_the_builder_share_one_lowering() {
    // A policy the Responses wire cannot express: refused at construction, by the
    // builder, and by `validate` with the same message.
    for policy in [ThinkingPolicy::Enabled, ThinkingPolicy::Preserved] {
        let profile = ModelProfile {
            thinking: policy,
            thinking_budgets: BTreeMap::new(),
            ..budget_profile()
        };
        let error = provider(profile.clone()).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(error.message.contains("effort-level"), "{error}");
        assert_eq!(
            build_request(
                &route(),
                "gpt-5.6-sol",
                &profile,
                &request(ModelOptions::default())
            )
            .unwrap_err(),
            error
        );
    }

    // A `budget` profile is refused here even though it is valid data and composes
    // on the Messages wire: this adapter encodes `effort-level` only.
    let profile = budget_profile();
    let error = provider(profile.clone()).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(error.message.contains("effort-level"), "{error}");
    assert_eq!(
        build_request(
            &route(),
            "gpt-5.6-sol",
            &profile,
            &request(ModelOptions::default())
        )
        .unwrap_err(),
        error
    );

    // An effort the profile does not list: the same message from `validate` and
    // the builder (the constructor composes; the REQUEST is what cannot).
    let profile = budget_profile_with_efforts(vec![Effort::Low, Effort::Medium, Effort::High]);
    let options = ModelOptions {
        reasoning_effort: Some(Effort::Max),
        ..ModelOptions::default()
    };
    assert_eq!(
        provider(profile.clone())
            .expect("the profile composes")
            .validate(&request(options.clone()))
            .unwrap_err(),
        build_request(&route(), "gpt-5.6-sol", &profile, &request(options)).unwrap_err()
    );

    // An output cap this account's route does not carry: the same message from
    // `validate` and the builder.
    let profile = budget_profile_with_efforts(vec![Effort::Low, Effort::Medium, Effort::High]);
    let options = ModelOptions {
        max_output_tokens: Some(4_096),
        ..ModelOptions::default()
    };
    let from_validate = provider(profile.clone())
        .expect("the profile composes")
        .validate(&request(options.clone()))
        .unwrap_err();
    let from_builder =
        build_request(&route(), "gpt-5.6-sol", &profile, &request(options)).unwrap_err();
    assert_eq!(from_validate, from_builder);
    assert!(from_validate.message.contains("max_output_tokens"));

    // Nothing in the model policy depends on the wire model name: two names, one body.
    let profile = budget_profile_with_efforts(vec![Effort::Low, Effort::Medium, Effort::High]);
    let options = ModelOptions {
        reasoning_effort: Some(Effort::High),
        ..ModelOptions::default()
    };
    let mut first = build_request(&route(), "gpt-5.6-sol", &profile, &request(options.clone()))
        .expect("the request builds");
    let second = build_request(&route(), "vendor/alias", &profile, &request(options))
        .expect("the same profile on another wire name builds");
    assert_eq!(first["reasoning"], second["reasoning"]);
    assert_eq!(first["include"], second["include"]);
    assert_eq!(first["text"], second["text"]);
    first["model"] = second["model"].clone();
    assert_eq!(first, second, "the wire name is the only difference");
}

/// [`budget_profile`] with an effort-level policy and the given efforts.
fn budget_profile_with_efforts(efforts: Vec<Effort>) -> ModelProfile {
    ModelProfile {
        thinking: ThinkingPolicy::EffortLevel,
        efforts,
        thinking_budgets: BTreeMap::new(),
        ..budget_profile()
    }
}

/// The adapter decides nothing by model NAME (spec §7.1): no model-id literal and no
/// name-shape rule survives in its PRODUCTION source. The unit tests at the end of
/// each file (`#[cfg(test)]`) may name models.
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

    // Name-SHAPE classifiers, the kind the Messages adapter had to delete: none of
    // them may appear here, now or later.
    let gone = [
        "is_adaptive",
        "model_kind",
        "model_prefix",
        "MODEL_PREFIXES",
    ];
    // Model names, not name-shaped substrings: `chatgpt-account-id` legitimately
    // contains "gpt-", so the shipped name prefixes are spelled out in full.
    let model_literals = [
        "gpt-5",
        "gpt-4",
        "gpt-test",
        "claude-opus",
        "claude-sonnet",
        "claude-fable",
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

/// The profile is the only source of the model policy: the same wire model lowers
/// differently for profiles whose effort lists differ, and the effort list — not the
/// model name — decides what is rejected.
#[test]
fn the_profiles_effort_list_decides_not_the_model_name() {
    let wire_model = "gpt-5.6-sol";
    let narrow = budget_profile_with_efforts(vec![Effort::High]);
    let wide = budget_profile_with_efforts(vec![
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::ExtraHigh,
    ]);
    let options = ModelOptions {
        reasoning_effort: Some(Effort::ExtraHigh),
        ..ModelOptions::default()
    };

    // The wide profile takes `extra_high` — spelled `xhigh` on this wire — while
    // the narrow one rejects it, same model name.
    let wide_body = build_request(&route(), wire_model, &wide, &request(options.clone()))
        .expect("the wide profile takes the effort");
    assert_eq!(wide_body["reasoning"]["effort"], serde_json::json!("xhigh"));
    let error = build_request(&route(), wire_model, &narrow, &request(options.clone()))
        .expect_err("the narrow profile rejects the effort");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(
        error,
        provider(narrow.clone())
            .expect("the narrow profile composes")
            .validate(&request(options))
            .unwrap_err()
    );

    // An effort-less request carries no reasoning fields and asks for no replay,
    // for every profile: the adapter's first-party behaviour, now from the profile
    // (no `default_effort`).
    let plain = ModelOptions::default();
    for profile in [&narrow, &wide] {
        let body = build_request(&route(), wire_model, profile, &request(plain.clone()))
            .expect("an effort-less request builds");
        assert!(body.get("reasoning").is_none(), "{body}");
        assert!(body.get("include").is_none(), "{body}");
    }
}
