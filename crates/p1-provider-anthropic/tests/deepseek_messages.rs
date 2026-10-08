use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::history::{AssistantBlock, ToolResultItem, ToolStatus};
use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, CompletedResponse, Effort, Item, ModelOptions,
    Outcome, Provider, ProviderError, ProviderRequest, StreamEvent,
};
use p1_model_profile::ModelProfile;
use p1_provider_anthropic::{
    AnthropicProvider, MessagesAccount, MessagesRoute, build_request, lower_request,
    validate_request,
};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use serde_json::{Value, json};

const MODEL: &str = "deepseek-v4.1-flash";
const THINKING: &str = "先检查\nquoted \"value\" ";
const SIGNATURE: &str = "sig/α== ";

fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: "anthropic-messages/opencode-go-messages".into(),
        endpoint: "https://opencode.ai/zen/go".into(),
        account: MessagesAccount::OpencodeGo,
        long_context: false,
    }
}

fn profile() -> ModelProfile {
    ModelProfile::from_toml(
        MODEL,
        include_str!("../../../profiles/deepseek-v4.1-flash.toml"),
    )
    .unwrap()
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "Coding prompt without a vendor identity".into(),
        history: vec![Item::User {
            text: "look up alpha and beta".into(),
        }],
        tools: vec![ToolDeclaration {
            name: "lookup".into(),
            description: "Find a label".into(),
            kind: DeclarationKind::Function {
                input_schema: json!({"type":"object","properties":{"label":{"type":"string"}},"required":["label"]}),
            },
        }],
        options: ModelOptions {
            max_output_tokens: Some(512),
            cache_key: Some("session-A".into()),
            ..Default::default()
        },
    }
}

#[test]
fn deepseek_body_and_headers_are_not_claude_requests() {
    let req = request();
    let lowered = lower_request(&route(), MODEL, &profile(), &req).unwrap();
    assert_eq!(lowered.path, "/v1/messages");
    assert_eq!(
        serde_json::from_slice::<Value>(&lowered.body).unwrap(),
        json!({
            "model": MODEL, "stream": true, "max_tokens": 512,
            "thinking": {"type":"enabled"}, "output_config":{"effort":"high"},
            "system":[{"type":"text","text":req.system_prompt}],
            "messages":[{"role":"user","content":[{"type":"text","text":"look up alpha and beta"}]}],
            "tools":[{"name":"lookup","description":"Find a label","input_schema":{"type":"object","properties":{"label":{"type":"string"}},"required":["label"]}}]
        })
    );
    let headers: std::collections::BTreeMap<_, _> = lowered.headers.into_iter().collect();
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["x-opencode-session"], "session-A");
    for forbidden in [
        "anthropic-beta",
        "x-app",
        "anthropic-dangerous-direct-browser-access",
        "authorization",
        "x-api-key",
    ] {
        assert!(!headers.contains_key(forbidden));
    }
    assert_eq!(route().describe(MODEL).mandatory_prompt_prefix, None);
    assert_eq!(route().describe(MODEL).cache_key, CacheKeySupport::Optional);

    for (effort, wire) in [(Effort::Low, "low"), (Effort::Max, "max")] {
        let mut req = req.clone();
        req.options.reasoning_effort = Some(effort);
        assert_eq!(
            build_request(&route(), MODEL, &profile(), &req).unwrap()["output_config"],
            json!({"effort": wire})
        );
    }
    let mut uncapped = req.clone();
    uncapped.options.max_output_tokens = None;
    assert_eq!(
        build_request(&route(), MODEL, &profile(), &uncapped).unwrap()["max_tokens"],
        384000
    );
    let mut without_limit = profile();
    without_limit.max_output_tokens = None;
    assert_eq!(
        build_request(&route(), MODEL, &without_limit, &uncapped).unwrap()["max_tokens"],
        256000
    );

    let mut disabled = profile();
    disabled.default_effort = None;
    let body = build_request(&route(), MODEL, &disabled, &req).unwrap();
    assert_eq!(body["thinking"], json!({"type":"disabled"}));
    assert!(body.get("output_config").is_none());
    // Profile-supported does not mean wire-supported: medium must not become high.
    disabled.efforts.push(Effort::Medium);
    let mut invalid = req.clone();
    invalid.options.reasoning_effort = Some(Effort::Medium);
    assert!(validate_request(&route(), MODEL, &disabled, &invalid).is_err());
    for key in ["", "session\r\ninjection"] {
        invalid.options.reasoning_effort = None;
        invalid.options.cache_key = Some(key.into());
        assert!(validate_request(&route(), MODEL, &profile(), &invalid).is_err());
    }
    let mut invalid_route = route();
    invalid_route.long_context = true;
    assert!(lower_request(&invalid_route, MODEL, &profile(), &req).is_err());
}

struct FakeKey;
impl CredentialSource for FakeKey {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: "FAKE-KEY-FOR-TEST".into(),
                account_id: None,
            })
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        self.access()
    }
}

fn response(tools: bool) -> ScriptedResponse {
    let mut events = vec![
        json!({"type":"message_start","message":{"model":"server-alias","usage":{"input_tokens":313,"cache_read_input_tokens":0,"cache_creation_input_tokens":13,"output_tokens":0}}}),
        json!({"type":"ping"}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"先","signature":"sig/"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"检查\nquoted \"value\" "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"α== "}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    if tools {
        for (index, label) in [(1, "alpha"), (2, "beta")] {
            events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("call-{index}"),"name":"lookup","input":{}}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":format!("{{\"label\":\"{label}\"}}")}}));
            events.push(json!({"type":"content_block_stop","index":index}));
        }
    } else {
        events.push(json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"OK"}}));
        events.push(json!({"type":"content_block_stop","index":1}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":if tools {"tool_use"} else {"end_turn"}},"usage":{"input_tokens":164,"cache_read_input_tokens":256,"output_tokens":2}}));
    events.push(json!({"type":"message_stop"}));
    let body: String = events
        .into_iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect();
    ScriptedResponse {
        status: 200,
        headers: vec![],
        chunks: body.as_bytes().chunks(7).map(<[u8]>::to_vec).collect(),
        end: BodyEnd::Eof,
    }
}

async fn complete(provider: &dyn Provider, req: ProviderRequest) -> CompletedResponse {
    let mut stream = provider
        .stream(req, CancellationToken::new())
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if let StreamEvent::Finished(outcome) = event {
            return match outcome {
                Outcome::Completed(completed) => completed,
                other => panic!("unexpected outcome: {other:?}"),
            };
        }
    }
    panic!("no terminal event");
}

#[tokio::test]
async fn deepseek_stream_replays_thinking_on_tool_and_non_tool_turns_and_counts_cache_once() {
    let transport = Arc::new(ScriptedTransport::new(vec![
        response(true),
        response(false),
    ]));
    let provider = AnthropicProvider::new(
        route(),
        MODEL,
        Arc::new(profile()),
        transport.clone(),
        Arc::new(FakeKey),
    )
    .unwrap();
    let mut req = request();
    let first = complete(&provider, req.clone()).await;
    let usage = first.usage.unwrap();
    assert_eq!(
        (
            usage.input_uncached,
            usage.cache_read,
            usage.cache_write,
            usage.output
        ),
        (Some(164), Some(256), Some(13), Some(2))
    );
    assert_eq!(first.item.origin, route().origin(MODEL));
    assert_eq!(first.item.blocks.len(), 3);
    req.history.push(Item::Assistant(first.item));
    for (id, content) in [("call-1", "value-alpha"), ("call-2", "value-beta")] {
        req.history.push(Item::ToolResult(ToolResultItem {
            call_id: id.into(),
            name: "lookup".into(),
            content: content.into(),
            status: ToolStatus::Ok,
        }));
    }
    let second = complete(&provider, req.clone()).await;
    req.history.push(Item::Assistant(second.item));
    req.history.push(Item::User {
        text: "continue".into(),
    });
    let body = build_request(&route(), MODEL, &profile(), &req).unwrap();
    for index in [1, 3] {
        assert_eq!(
            body["messages"][index]["content"][0],
            json!({"type":"thinking","thinking":THINKING,"signature":SIGNATURE})
        );
    }
    assert_eq!(body["messages"][2]["content"][1]["tool_use_id"], "call-2");
    assert_eq!(body["messages"][2]["content"][1]["content"], "value-beta");
    // Same model, different origin: never replay foreign reasoning.
    // Locate the final assistant by variant rather than rely on tool count above.
    for item in req.history.iter_mut().rev() {
        if let Item::Assistant(item) = item {
            if let AssistantBlock::Reasoning {
                replay: Some(data), ..
            } = &mut item.blocks[0]
            {
                data.origin.route = "foreign-route".into();
            }
            break;
        }
    }
    let foreign = build_request(&route(), MODEL, &profile(), &req).unwrap();
    assert_eq!(
        foreign["messages"][3]["content"],
        json!([{"type":"text","text":"OK"}])
    );
    for posted in transport.requests() {
        assert_eq!(posted.url, "https://opencode.ai/zen/go/v1/messages");
        let headers: std::collections::BTreeMap<_, _> = posted.headers.into_iter().collect();
        assert_eq!(headers["x-api-key"], "FAKE-KEY-FOR-TEST");
        assert_eq!(headers["authorization"], "Bearer FAKE-KEY-FOR-TEST");
        assert_eq!(headers["x-opencode-session"], "session-A");
        assert!(!headers.contains_key("anthropic-beta"));
    }
}
