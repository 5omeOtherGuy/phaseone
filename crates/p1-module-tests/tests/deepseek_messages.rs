#[path = "../../p1-host/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, Item, ModelOptions, Outcome, ProviderError,
    ProviderRequest, StreamEvent,
};
use p1_host::routes::load_route;
use p1_model_profile::ModelProfile;
use p1_provider_http::testing::{
    BodyEnd, ScriptedResponse, ScriptedTransport, ScriptedWsConnector,
};
use p1_provider_http::{Credential, CredentialSource};
use serde_json::{Value, json};

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

struct FakeKey;
impl CredentialSource for FakeKey {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: "MODULE-FAKE-KEY".into(),
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

#[tokio::test]
async fn messages_guest_and_broker_preserve_deepseek_wire_auth_and_replay() {
    let model = "deepseek-v4.1-flash";
    let route = load_route(&repo("routes/opencode-go-messages.toml")).unwrap();
    let profile = Arc::new(
        ModelProfile::from_toml(
            model,
            &std::fs::read_to_string(repo("profiles/deepseek-v4.1-flash.toml")).unwrap(),
        )
        .unwrap(),
    );
    let body: String = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":164,"cache_read_input_tokens":256,"cache_creation_input_tokens":0,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"reason\n ","signature":"signature== "}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"OK"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ].into_iter().map(|event| format!("data: {event}\n\n")).collect();
    let response = ScriptedResponse {
        status: 200,
        headers: vec![],
        chunks: vec![body.into_bytes()],
        end: BodyEnd::Eof,
    };
    let transport = ScriptedTransport::new(vec![response.clone(), response]);
    let provider = common::provider_components()
        .activate(
            &[repo("environments")],
            &route,
            profile,
            Arc::new(transport.clone()),
            Arc::new(ScriptedWsConnector::new(vec![])),
            Arc::new(FakeKey),
        )
        .unwrap();
    assert_eq!(provider.describe().mandatory_prompt_prefix, None);
    assert_eq!(provider.describe().cache_key, CacheKeySupport::Optional);
    let mut request = ProviderRequest {
        system_prompt: "SYS".into(),
        history: vec![Item::User {
            text: "Reply OK".into(),
        }],
        tools: vec![],
        options: ModelOptions {
            max_output_tokens: Some(128),
            cache_key: Some("module-session".into()),
            ..Default::default()
        },
    };
    for turn in 0..2 {
        let events: Vec<_> = provider
            .stream(request.clone(), CancellationToken::new())
            .await
            .unwrap()
            .collect()
            .await;
        let StreamEvent::Finished(Outcome::Completed(completed)) = events.last().unwrap() else {
            panic!("expected completed response")
        };
        let usage = completed.usage.as_ref().unwrap();
        assert_eq!(
            (
                usage.input_uncached,
                usage.cache_read,
                usage.cache_write,
                usage.output
            ),
            (Some(164), Some(256), Some(0), Some(2))
        );
        if turn == 0 {
            request
                .history
                .push(Item::Assistant(completed.item.clone()));
            request.history.push(Item::User {
                text: "continue".into(),
            });
        }
    }
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({"type":"thinking","thinking":"reason\n ","signature":"signature== "})
    );
    assert_eq!(body["system"], json!([{"type":"text","text":"SYS"}]));
    assert_eq!(body["thinking"], json!({"type":"enabled"}));
    assert_eq!(body["output_config"], json!({"effort":"high"}));
    for request in requests {
        assert_eq!(request.url, "https://opencode.ai/zen/go/v1/messages");
        let headers: std::collections::BTreeMap<_, _> = request.headers.into_iter().collect();
        assert_eq!(headers["x-api-key"], "MODULE-FAKE-KEY");
        assert_eq!(headers["authorization"], "Bearer MODULE-FAKE-KEY");
        assert_eq!(headers["x-opencode-session"], "module-session");
        assert!(!headers.contains_key("anthropic-beta"));
    }
}
