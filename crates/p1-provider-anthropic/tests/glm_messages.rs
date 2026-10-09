use p1_contracts::{Effort, Item, ModelOptions, ProviderRequest};
use p1_model_profile::ModelProfile;
use p1_provider_anthropic::{MessagesAccount, MessagesRoute, build_headers, lower_request};
use p1_provider_http::Credential;
use serde_json::{Value, json};

#[test]
fn glm_messages_lowers_both_profiles_to_enabled_thinking_and_output_effort() {
    let route = MessagesRoute {
        origin_route: "anthropic-messages/glm-messages@zai".into(),
        endpoint: "https://api.z.ai/api/anthropic".into(),
        account: MessagesAccount::Zai,
        long_context: false,
    };
    for (id, text) in [
        ("glm-5.3", include_str!("../../../profiles/glm-5.3.toml")),
        (
            "glm-5.3-flash",
            include_str!("../../../profiles/glm-5.3-flash.toml"),
        ),
    ] {
        let mut profile = ModelProfile::from_toml(id, text).unwrap();
        for (effort, wire) in [
            (None, "high"),
            (Some(Effort::Low), "low"),
            (Some(Effort::High), "high"),
            (Some(Effort::Max), "max"),
        ] {
            let request = ProviderRequest {
                system_prompt: "Test system".into(),
                history: vec![Item::User {
                    text: "Test user".into(),
                }],
                tools: vec![],
                options: ModelOptions {
                    reasoning_effort: effort,
                    ..Default::default()
                },
            };
            let lowered = lower_request(&route, id, &profile, &request).unwrap();
            assert_eq!(
                format!("{}{}", route.endpoint, lowered.path),
                "https://api.z.ai/api/anthropic/v1/messages"
            );
            let body: Value = serde_json::from_slice(&lowered.body).unwrap();
            assert_eq!(
                body,
                json!({
                    "model": id, "stream": true, "max_tokens": 131072,
                    "thinking": {"type": "enabled"}, "output_config": {"effort": wire},
                    "system": [{"type": "text", "text": "Test system"}],
                    "messages": [{"role": "user", "content": [{"type": "text", "text": "Test user"}]}]
                })
            );
            let headers: std::collections::BTreeMap<_, _> = build_headers(
                MessagesAccount::Zai,
                &Credential {
                    bearer: "FAKE-test".into(),
                    account_id: None,
                },
                &body,
            )
            .into_iter()
            .collect();
            assert_eq!(headers["anthropic-version"], "2023-06-01");
            assert_eq!(headers["x-api-key"], "FAKE-test");
            assert_eq!(headers["authorization"], "Bearer FAKE-test");
            for forbidden in ["anthropic-beta", "x-opencode-session", "x-app"] {
                assert!(!headers.contains_key(forbidden));
            }
            assert!(
                lower_request(
                    &route,
                    id,
                    &profile,
                    &ProviderRequest {
                        options: ModelOptions {
                            reasoning_effort: Some(Effort::Medium),
                            ..Default::default()
                        },
                        ..request.clone()
                    }
                )
                .is_err()
            );
            // No default effort must never turn thinking off on Z.ai.
            profile.default_effort = None;
            let omitted = lower_request(
                &route,
                id,
                &profile,
                &ProviderRequest {
                    options: ModelOptions::default(),
                    ..request
                },
            )
            .unwrap();
            let omitted: Value = serde_json::from_slice(&omitted.body).unwrap();
            assert_eq!(omitted["thinking"], json!({"type": "enabled"}));
            assert!(omitted.get("output_config").is_none());
            profile.default_effort = Some(Effort::High);
        }
    }
}
