//! Hand-written fixtures only; each subscription passes THE shared suite unchanged.
use p1_contracts::{BoxFuture, Effort, ModelOptions, Provider, ProviderError, ProviderRequest};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_conformance::{RouteFixtures, RouteUnderTest, run_all};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai_chat::{ChatDialect, ChatLimits, ChatProvider, ChatRoute, build_request};
use std::sync::Arc;
const MODEL: &str = "configured-model";
const BEARER: &str = "CONFORMANCE-FAKE-BEARER";
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

fn config(retained: bool) -> (ChatRoute, Arc<ModelProfile>) {
    (
        ChatRoute {
            origin_route: if retained {
                "openai-chat/glm-subscription"
            } else {
                "openai-chat/opencode-go-subscription"
            }
            .into(),
            endpoint: if retained {
                "https://api.z.ai/api/coding/paas/v4/chat/completions"
            } else {
                "https://opencode.ai/zen/go/v1/chat/completions"
            }
            .into(),
            headers: vec![("user-agent".into(), "p1/test".into())],
            session_header: (!retained).then(|| "x-opencode-session".into()),
            dialect: if retained {
                ChatDialect::RetainedThinking
            } else {
                ChatDialect::ThinkingWithReasoningAlias
            },
            limits: ChatLimits::default(),
        },
        Arc::new(ModelProfile {
            model_id: "canonical-model".into(),
            thinking: if retained {
                ThinkingPolicy::Preserved
            } else {
                ThinkingPolicy::Enabled
            },
            efforts: if retained {
                vec![Effort::Low, Effort::High, Effort::Max]
            } else {
                vec![Effort::High, Effort::Max]
            },
            default_effort: Effort::High,
            max_output_tokens: retained.then_some(131_072),
        }),
    )
}
fn provider(retained: bool, t: ScriptedTransport) -> Arc<dyn Provider> {
    let (route, profile) = config(retained);
    Arc::new(ChatProvider::new(route, MODEL, profile, Arc::new(t), Arc::new(Fixed)).unwrap())
}
fn go(t: ScriptedTransport) -> Arc<dyn Provider> {
    provider(false, t)
}
fn glm(t: ScriptedTransport) -> Arc<dyn Provider> {
    provider(true, t)
}
fn go_request(r: &ProviderRequest) -> serde_json::Value {
    let (route, profile) = config(false);
    build_request(&route, MODEL, &profile, r).unwrap()
}
fn glm_request(r: &ProviderRequest) -> serde_json::Value {
    let (route, profile) = config(true);
    build_request(&route, MODEL, &profile, r).unwrap()
}
fn invalid() -> ProviderRequest {
    ProviderRequest {
        system_prompt: String::new(),
        history: vec![],
        tools: vec![],
        options: ModelOptions {
            reasoning_effort: Some(Effort::Medium),
            ..ModelOptions::default()
        },
    }
}
fn fixtures() -> RouteFixtures {
    RouteFixtures {
        text_turn: include_str!("fixtures/text.sse"),
        tool_call_turn: include_str!("fixtures/tool.sse"),
        two_tool_calls: include_str!("fixtures/two_tools.sse"),
        truncated_tool_call: include_str!("fixtures/truncated.sse"),
        invalid_tool_json: include_str!("fixtures/invalid_json.sse"),
        error_event: include_str!("fixtures/error.sse"),
        no_usage: include_str!("fixtures/no_usage.sse"),
        reasoning_turn: include_str!("fixtures/reasoning.sse"),
        events_after_terminal: include_str!("fixtures/after_terminal.sse"),
    }
}
#[test]
fn opencode_go_conformance() {
    run_all(&RouteUnderTest {
        name: "opencode-go",
        build: go,
        fixtures: fixtures(),
        follow_up_request: go_request,
        fake_bearer: BEARER,
        invalid_request: invalid,
    });
}
#[test]
fn glm_conformance() {
    run_all(&RouteUnderTest {
        name: "glm",
        build: glm,
        fixtures: fixtures(),
        follow_up_request: glm_request,
        fake_bearer: BEARER,
        invalid_request: invalid,
    });
}

#[tokio::test]
async fn subscription_endpoint_and_session_header_are_route_scoped() {
    use futures_util::StreamExt;
    use p1_provider_http::testing::ScriptedResponse;
    for retained in [false, true] {
        let (route, profile) = config(retained);
        let transport = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(include_str!(
            "fixtures/no_usage.sse"
        ))]);
        let provider = ChatProvider::new(
            route.clone(),
            MODEL,
            profile,
            Arc::new(transport.clone()),
            Arc::new(Fixed),
        )
        .unwrap();
        let mut r = invalid();
        r.options = ModelOptions::default();
        if !retained {
            r.options.cache_key = Some("synthetic-session".into());
        }
        let mut stream = provider
            .stream(r, p1_contracts::CancellationToken::new())
            .await
            .unwrap();
        while stream.next().await.is_some() {}
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, route.endpoint);
        let session = requests[0]
            .headers
            .iter()
            .find(|(name, _)| name == "x-opencode-session")
            .map(|(_, value)| value.as_str());
        assert_eq!(
            session,
            if !retained {
                Some("synthetic-session")
            } else {
                None
            }
        );
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "user-agent" && value.starts_with("p1/"))
        );
        assert!(!format!("{provider:?}").contains(BEARER));
    }
}

#[test]
fn one_profile_has_identical_policy_on_two_compatible_routes() {
    let (first, profile) = config(false);
    let mut second = first.clone();
    second.origin_route = "synthetic/second-account".into();
    second.endpoint = "https://other.example.test/v1/chat/completions".into();
    second.session_header = Some("x-other-session".into());
    let mut request = invalid();
    request.options = ModelOptions::default();
    request.options.reasoning_effort = Some(Effort::Max);
    request.options.max_output_tokens = Some(500);
    assert_eq!(
        build_request(&first, "wire-alias", &profile, &request).unwrap(),
        build_request(&second, "wire-alias", &profile, &request).unwrap()
    );
    assert_ne!(first.origin("wire-alias"), second.origin("wire-alias"));
    second.limits.max_output_tokens = Some(499);
    assert!(build_request(&second, "wire-alias", &profile, &request).is_err());
    assert!(build_request(&first, "wire-alias", &profile, &request).is_ok());
}

#[test]
fn incompatible_policy_and_unsafe_route_configuration_fail_at_construction() {
    let (base, _) = config(false);
    let (_, retained) = config(true);
    let make = |route, profile| {
        ChatProvider::new(
            route,
            MODEL,
            profile,
            Arc::new(ScriptedTransport::new(vec![])),
            Arc::new(Fixed),
        )
    };
    assert!(make(base.clone(), retained).is_err());
    for (name, value) in [
        ("Authorization", "FAKE-secret"),
        ("Cookie", "FAKE-secret"),
        ("x-api-key", "FAKE-secret"),
        ("X-Test", "FAKE\nsecret"),
    ] {
        let mut route = base.clone();
        route.headers.push((name.into(), value.into()));
        let error = make(route, config(false).1).unwrap_err();
        assert!(!error.to_string().contains("FAKE"));
    }
    for endpoint in [
        "https://user:FAKE-secret@example.test/v1",
        "https://example.test/v1?key=FAKE-secret",
    ] {
        let mut route = base.clone();
        route.endpoint = endpoint.into();
        assert!(!format!("{route:?}").contains("FAKE"));
        let error = make(route, config(false).1).unwrap_err();
        assert!(!error.to_string().contains("FAKE"));
    }
}

#[tokio::test]
async fn replay_is_structurally_preserved_in_the_actual_second_request() {
    use futures_util::StreamExt;
    use p1_contracts::{Item, Outcome, StreamEvent, ToolResultItem, ToolStatus};
    use p1_provider_http::testing::ScriptedResponse;
    for retained in [false, true] {
        let transcript = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"reasoning_content\":\"exact reasoning 雪\"}},\"finish_reason\":null}}]}}\n\n{}",
            include_str!("fixtures/tool.sse")
        );
        let transport = ScriptedTransport::new(vec![
            ScriptedResponse::ok_sse(&transcript),
            ScriptedResponse::ok_sse(include_str!("fixtures/no_usage.sse")),
        ]);
        let p = provider(retained, transport.clone());
        let mut r = invalid();
        r.options = ModelOptions::default();
        r.history = vec![Item::User {
            text: "synthetic question".into(),
        }];
        let mut stream = p
            .stream(r.clone(), p1_contracts::CancellationToken::new())
            .await
            .unwrap();
        let mut item = None;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Finished(Outcome::Completed(done)) = event {
                item = Some(done.item);
            }
        }
        let item = item.expect("completed reasoning and tool call");
        let call = item.tool_calls().next().unwrap().clone();
        r.history.push(Item::Assistant(item));
        r.history.push(Item::ToolResult(ToolResultItem {
            call_id: call.call_id,
            name: call.name,
            status: ToolStatus::Ok,
            content: "result".into(),
        }));
        let mut stream = p
            .stream(r, p1_contracts::CancellationToken::new())
            .await
            .unwrap();
        while stream.next().await.is_some() {}
        let requests = transport.requests();
        let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let message = &body["messages"][2];
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["reasoning_content"], "exact reasoning 雪");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.txt\"}"
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
    }
}
