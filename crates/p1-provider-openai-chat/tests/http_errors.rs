//! An exhausted account is diagnosed as such (ADR-0046). Synthetic error bodies
//! only: the live shape is unknown, so every case below is a guess about the wire
//! that the adapter must either recognise or fall back on safely.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, ModelOptions, Outcome, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, StreamEvent,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai_chat::{ChatDialect, ChatLimits, ChatProvider, ChatRoute};

const MODEL: &str = "configured-model";
const BEARER: &str = "HTTP-ERRORS-FAKE-BEARER";
/// A string that appears in a body and must never appear in an error.
const SENTINEL: &str = "SENTINEL-SERVER-TEXT";

const NO_BALANCE_MESSAGE: &str = "the account has no balance";
const NO_BALANCE_WORDS: [&str; 5] = [
    "creditserror",
    "insufficient_balance",
    "insufficient_quota",
    "quota_exceeded",
    "billing_error",
];
const NOT_ENTITLED_MESSAGE: &str = "the account's plan does not allow this model on this route";
const NOT_ENTITLED_WORDS: [&str; 3] = ["freetiererror", "not_entitled", "plan_not_allowed"];
/// The four fixed positions a no-balance word may occupy, as JSON pointers.
const POSITIONS: [&str; 4] = ["error/type", "error/code", "type", "code"];

#[derive(Default)]
struct CountingCredential {
    /// Credentials that only the adapter gets to refresh.
    refreshes: AtomicUsize,
}

impl CredentialSource for CountingCredential {
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
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}

/// A chat provider wired to `responses`, with a credential source that counts the
/// refreshes a rejection triggers.
fn provider(
    responses: Vec<ScriptedResponse>,
) -> (
    Arc<dyn Provider>,
    ScriptedTransport,
    Arc<CountingCredential>,
) {
    let transport = ScriptedTransport::new(responses);
    let credentials = Arc::new(CountingCredential::default());
    let route = ChatRoute {
        origin_route: "openai-chat/exhausted-account".into(),
        endpoint: "https://opencode.example.test/zen/go/v1/chat/completions".into(),
        headers: vec![],
        session_header: None,
        dialect: ChatDialect::ThinkingWithReasoningAlias,
        limits: ChatLimits::default(),
    };
    let profile = ModelProfile {
        id: "canonical-model".into(),
        revision: 1,
        model_id: "canonical-model".into(),
        family: "test".into(),
        thinking: ThinkingPolicy::Enabled,
        efforts: vec![Effort::High, Effort::Max],
        default_effort: Some(Effort::High),
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    };
    let provider = ChatProvider::new(
        route,
        MODEL,
        Arc::new(profile),
        Arc::new(transport.clone()),
        credentials.clone(),
    )
    .expect("a valid route and profile");
    (Arc::new(provider), transport, credentials)
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "synthetic system prompt".into(),
        history: vec![],
        tools: vec![],
        options: ModelOptions::default(),
    }
}

/// An error response: `status` with `body` as its single body chunk.
fn error_response(status: u16, body: &[u8]) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: Vec::new(),
        chunks: vec![body.to_vec()],
        end: BodyEnd::Eof,
    }
}

/// One body naming `word` at `position` and nowhere else.
fn error_body(position: &str, word: &str) -> Vec<u8> {
    let body = match position {
        "error/type" => serde_json::json!({"error": {"type": word}}),
        "error/code" => serde_json::json!({"error": {"code": word}}),
        "type" => serde_json::json!({"type": word}),
        "code" => serde_json::json!({"code": word}),
        other => panic!("unknown position {other}"),
    };
    serde_json::to_vec(&body).unwrap()
}

/// Run one request to its terminal event.
async fn finish(provider: &Arc<dyn Provider>) -> Outcome {
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request builds");
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Finished(outcome) = event {
            terminal = Some(outcome);
        }
    }
    terminal.expect("exactly one terminal event")
}

fn failed(outcome: Outcome) -> ProviderError {
    match outcome {
        Outcome::Failed(error) => error,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn each_allow_listed_word_in_any_of_the_four_positions_is_insufficient_balance() {
    for word in NO_BALANCE_WORDS {
        for position in POSITIONS {
            let body = error_body(position, word);
            let (provider, transport, credentials) = provider(vec![error_response(401, &body)]);
            let error = failed(finish(&provider).await);

            assert_eq!(
                error.kind,
                ProviderErrorKind::InsufficientBalance,
                "{position} {word}"
            );
            assert_eq!(error.message, NO_BALANCE_MESSAGE, "{position} {word}");
            assert_eq!(
                transport.requests().len(),
                1,
                "{position} {word}: no refresh and no re-send"
            );
            assert_eq!(
                credentials.refreshes.load(Ordering::SeqCst),
                0,
                "{position} {word}: the credential is not refreshed"
            );
        }
    }
}

#[tokio::test]
async fn each_plan_refusal_word_in_any_position_is_not_entitled() {
    for word in NOT_ENTITLED_WORDS {
        for position in POSITIONS {
            let body = error_body(position, word);
            let (provider, transport, credentials) = provider(vec![error_response(403, &body)]);
            let error = failed(finish(&provider).await);

            assert_eq!(
                error.kind,
                ProviderErrorKind::NotEntitled,
                "{position} {word}"
            );
            assert_eq!(error.message, NOT_ENTITLED_MESSAGE, "{position} {word}");
            assert_eq!(
                transport.requests().len(),
                1,
                "{position} {word}: no refresh and no re-send"
            );
            assert_eq!(
                credentials.refreshes.load(Ordering::SeqCst),
                0,
                "{position} {word}: the credential is not refreshed"
            );
        }
    }
}

#[tokio::test]
async fn the_free_tier_error_shape_is_not_entitled_and_hides_the_server_text() {
    // The observed live shape: HTTP 403 for a model gated to OpenCode's client,
    // with a valid key. Case-insensitive, and the free text is never copied.
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {"type": "fReEtIeReRrOr", "message": SENTINEL},
    }))
    .unwrap();
    let (provider, transport, credentials) = provider(vec![error_response(403, &body)]);
    let error = failed(finish(&provider).await);

    assert_eq!(error.kind, ProviderErrorKind::NotEntitled);
    assert_eq!(error.message, NOT_ENTITLED_MESSAGE);
    assert!(
        !format!("{error} {error:?}").contains(SENTINEL),
        "the body text leaked into {error:?}"
    );
    assert_eq!(transport.requests().len(), 1);
    assert_eq!(credentials.refreshes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn matching_ignores_the_case_of_the_word() {
    for word in [
        "CreditsError",
        "INSUFFICIENT_BALANCE",
        "Insufficient_Quota",
        "Quota_Exceeded",
        "BILLING_ERROR",
    ] {
        let body = error_body("error/type", word);
        let (provider, transport, credentials) = provider(vec![error_response(403, &body)]);
        let error = failed(finish(&provider).await);

        assert_eq!(error.kind, ProviderErrorKind::InsufficientBalance, "{word}");
        assert_eq!(error.message, NO_BALANCE_MESSAGE, "{word}");
        assert_eq!(transport.requests().len(), 1, "{word}");
        assert_eq!(credentials.refreshes.load(Ordering::SeqCst), 0, "{word}");
    }
}

#[tokio::test]
async fn an_unrecognised_body_keeps_todays_authentication_error() {
    let cases: [(&str, &[u8]); 6] = [
        (
            "non-listed word",
            br#"{"error":{"type":"invalid_api_key","code":"bad_key"}}"#,
        ),
        (
            "non-string value",
            br#"{"error":{"type":7,"code":["quota_exceeded"]}}"#,
        ),
        ("non-JSON body", b"<html>gateway said no</html>"),
        ("empty body", b""),
        (
            "a position this adapter does not read",
            br#"{"error":{"message":"quota_exceeded"}}"#,
        ),
        (
            "a deeper position",
            br#"{"error":{"details":{"code":"insufficient_quota"}}}"#,
        ),
    ];
    for (name, body) in cases {
        // Two responses: the rejection, then the re-send after the one refresh.
        let script = vec![error_response(401, body), error_response(401, body)];
        let (provider, transport, credentials) = provider(script);
        let error = failed(finish(&provider).await);

        assert_eq!(error.kind, ProviderErrorKind::Authentication, "{name}");
        // A short token-shaped code is named (as the sibling adapters do); nothing else is.
        let expected = if name == "non-listed word" {
            "chat HTTP status 401 (bad_key)"
        } else {
            "chat HTTP status 401"
        };
        assert_eq!(error.message, expected, "{name}");
        assert_eq!(
            transport.requests().len(),
            2,
            "{name}: one refresh, one re-send"
        );
        assert_eq!(credentials.refreshes.load(Ordering::SeqCst), 1, "{name}");
    }
}

#[tokio::test]
async fn the_server_text_never_reaches_the_error() {
    let matched = serde_json::to_vec(&serde_json::json!({
        "error": {
            "type": "insufficient_balance",
            "code": SENTINEL,
            "message": SENTINEL,
        },
        "type": SENTINEL,
        "code": SENTINEL,
    }))
    .unwrap();
    let unmatched = serde_json::to_vec(&serde_json::json!({
        "error": {"type": "invalid_api_key", "message": SENTINEL},
        "detail": SENTINEL,
    }))
    .unwrap();

    let cases = [
        (
            "a recognised word",
            vec![error_response(401, &matched)],
            ProviderErrorKind::InsufficientBalance,
        ),
        (
            "an unrecognised word",
            vec![
                error_response(401, &unmatched),
                error_response(401, &unmatched),
            ],
            ProviderErrorKind::Authentication,
        ),
    ];
    for (name, script, kind) in cases {
        let (provider, _, _) = provider(script);
        let error = failed(finish(&provider).await);

        assert_eq!(error.kind, kind, "{name}");
        let rendered = format!("{error} {error:?}");
        assert!(
            !error.message.contains(SENTINEL) && !rendered.contains(SENTINEL),
            "{name}: the body text leaked into {rendered}"
        );
    }
}

#[tokio::test]
async fn a_refused_request_names_a_short_code_and_never_free_text() {
    let cases: [(&[u8], &str); 4] = [
        (
            br#"{"error":{"message":"SENTINEL free text","type":"invalid_request_error","code":"string_above_max_length"}}"#,
            "chat HTTP status 400 (string_above_max_length)",
        ),
        (
            br#"{"error":{"message":"SENTINEL free text","type":"invalid_request_error"}}"#,
            "chat HTTP status 400 (invalid_request_error)",
        ),
        (
            br#"{"error":{"code":"this code is a whole SENTINEL sentence, not a token"}}"#,
            "chat HTTP status 400",
        ),
        (b"SENTINEL not json", "chat HTTP status 400"),
    ];
    for (body, expected) in cases {
        let (provider, _transport, _credentials) = provider(vec![error_response(400, body)]);
        let error = failed(finish(&provider).await);
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(error.message, expected);
        assert!(!format!("{error} {error:?}").contains("SENTINEL"));
    }
}
