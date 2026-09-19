use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, CompletedResponse, Item, Origin,
    Outcome, Provider, ProviderError, ProviderErrorKind, ProviderRequest, ProviderStream,
    ReplayData, RouteDescription, StopReason, StreamEvent, ToolCall, ToolInput, Usage,
};
use p1_provider_conformance::*;
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{
    Credential, CredentialSource, DriveRequest, HttpRequest, ResponseParser, RetryPolicy, SseEvent,
    Transport, drive,
};

const BEARER: &str = "FAKE_CONFORMANCE_BEARER";
const ORIGIN: &str = "reference/line";
const MODEL: &str = "reference-model";

const FIXTURES: RouteFixtures = RouteFixtures {
    text_turn: "data: text Hello\n\ndata: text  world\n\ndata: usage 3 2\n\ndata: stop end\n\n",
    tool_call_turn: "data: text Looking\n\ndata: call call_1 read {\"path\":\"a.txt\"}\n\ndata: stop tool\n\n",
    two_tool_calls: "data: call call_1 read {\"path\":\"a.txt\"}\n\ndata: call call_2 grep {\"pattern\":\"x\"}\n\ndata: stop tool\n\n",
    truncated_tool_call: "data: partial call_1 read {\"path\":",
    invalid_tool_json: "data: call call_1 read {\"path\": \n\ndata: stop tool\n\n",
    error_event: "data: error response-body-sentinel\n\n",
    no_usage: "data: text Hello\n\ndata: text  world\n\ndata: stop end\n\n",
    reasoning_turn: "data: reason thought replay-leaf\n\ndata: text Hello\n\ndata: text  world\n\ndata: stop end\n\n",
    events_after_terminal: "data: text Hello\n\ndata: text  world\n\ndata: stop end\n\ndata: text STRAY\n\n",
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bug {
    None,
    Text,
    Tool,
    Order,
    Truncated,
    InvalidJson,
    Error,
    Usage,
    Replay,
    AfterTerminal,
    Chunking,
    Cancel,
    Http401,
    Http429,
    Http500,
    Http400,
    RetryVisible,
    Setup,
    AcceptsInvalid,
    CredentialLeak,
}

struct FixedCredential;
impl CredentialSource for FixedCredential {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.to_string(),
                account_id: None,
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}

struct LineParser {
    bug: Bug,
    blocks: Vec<AssistantBlock>,
    usage: Option<Usage>,
    partial: Option<ToolCall>,
}

impl LineParser {
    fn new(bug: Bug) -> Self {
        Self {
            bug,
            blocks: Vec::new(),
            usage: None,
            partial: None,
        }
    }

    fn response(&self, stop: StopReason) -> CompletedResponse {
        let mut blocks = self.blocks.clone();
        if self.bug == Bug::Order {
            blocks.reverse();
        }
        CompletedResponse {
            item: AssistantItem {
                origin: origin(),
                blocks,
            },
            stop,
            usage: if self.bug == Bug::Usage {
                Some(Usage {
                    input_uncached: Some(0),
                    output: Some(0),
                    ..Usage::default()
                })
            } else {
                self.usage
            },
        }
    }
}

fn origin() -> Origin {
    Origin {
        route: ORIGIN.to_string(),
        model: MODEL.to_string(),
    }
}

impl ResponseParser for LineParser {
    fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent> {
        let (command, rest) = event.data.split_once(' ').unwrap_or((&event.data, ""));
        match command {
            "text" => {
                let text = if self.bug == Bug::Text {
                    format!("wrong-{rest}")
                } else {
                    rest.to_string()
                };
                let block = match self.blocks.last_mut() {
                    Some(AssistantBlock::Text { text: accumulated }) => {
                        accumulated.push_str(&text);
                        self.blocks.len() - 1
                    }
                    _ => {
                        self.blocks
                            .push(AssistantBlock::Text { text: text.clone() });
                        self.blocks.len() - 1
                    }
                };
                if self.bug == Bug::RetryVisible {
                    vec![StreamEvent::Activity]
                } else {
                    vec![StreamEvent::TextDelta { block, text }]
                }
            }
            "call" | "partial" => {
                let mut fields = rest.splitn(3, ' ');
                let call = ToolCall {
                    call_id: fields.next().unwrap_or_default().to_string(),
                    name: fields.next().unwrap_or_default().to_string(),
                    input: ToolInput::Json(fields.next().unwrap_or_default().to_string()),
                };
                if command == "partial" {
                    self.partial = Some(call);
                } else {
                    let call = if self.bug == Bug::InvalidJson {
                        ToolCall {
                            input: ToolInput::Json("{}".to_string()),
                            ..call
                        }
                    } else if self.bug == Bug::Tool {
                        ToolCall {
                            call_id: "wrong".to_string(),
                            ..call
                        }
                    } else {
                        call
                    };
                    self.blocks.push(AssistantBlock::ToolCall(call));
                }
                Vec::new()
            }
            "reason" => {
                let (text, leaf) = rest.split_once(' ').unwrap_or((rest, ""));
                let replay = if self.bug == Bug::Replay {
                    None
                } else {
                    Some(ReplayData {
                        origin: origin(),
                        version: 1,
                        payload: serde_json::json!({"opaque": leaf}),
                    })
                };
                self.blocks.push(AssistantBlock::Reasoning {
                    text: text.to_string(),
                    replay,
                });
                vec![StreamEvent::ReasoningDelta {
                    block: self.blocks.len() - 1,
                    text: text.to_string(),
                }]
            }
            "usage" => {
                let numbers: Vec<u64> = rest
                    .split_whitespace()
                    .filter_map(|part| part.parse().ok())
                    .collect();
                self.usage = Some(Usage {
                    input_uncached: numbers.first().copied(),
                    output: numbers.get(1).copied(),
                    ..Usage::default()
                });
                Vec::new()
            }
            "stop" => {
                let stop = if rest == "tool" {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                };
                vec![StreamEvent::Finished(Outcome::Completed(
                    self.response(stop),
                ))]
            }
            "error" => {
                if self.bug == Bug::Error {
                    vec![StreamEvent::Finished(Outcome::Failed(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        event.data,
                    )))]
                } else {
                    vec![StreamEvent::Finished(Outcome::Failed(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "provider error event",
                    )))]
                }
            }
            _ => Vec::new(),
        }
    }

    fn on_end(&mut self) -> Outcome {
        if self.bug == Bug::Truncated {
            if let Some(call) = self.partial.take() {
                self.blocks.push(AssistantBlock::ToolCall(call));
            }
            Outcome::Completed(self.response(StopReason::ToolUse))
        } else {
            Outcome::Failed(ProviderError::new(
                ProviderErrorKind::Transport,
                "stream ended before stop",
            ))
        }
    }

    fn on_http_error(
        &self,
        status: u16,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> ProviderError {
        let mut kind = match status {
            401 | 403 => ProviderErrorKind::Authentication,
            408 | 425 | 429 | 500..=599 => ProviderErrorKind::Transport,
            _ => ProviderErrorKind::InvalidRequest,
        };
        if self.bug == Bug::Http400 && status == 400 {
            kind = ProviderErrorKind::Transport;
        }
        let message = if self.bug == Bug::CredentialLeak {
            format!("status {status} {BEARER}")
        } else {
            format!("status {status}")
        };
        ProviderError::new(kind, message)
    }
}

struct ReferenceProvider {
    transport: ScriptedTransport,
    bug: Bug,
    chunk_alter: bool,
}

impl Provider for ReferenceProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: origin(),
            supports_freeform_tools: false,
            mandatory_prompt_prefix: None,
            reports_cost: false,
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        // The reference route cannot carry an output cap.
        if request.options.max_output_tokens.is_some() && self.bug != Bug::AcceptsInvalid {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "this route does not accept max_output_tokens",
            ));
        }
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        if let Err(error) = self.validate(&request) {
            return Box::pin(async { Err(error) });
        }
        if self.bug == Bug::Setup {
            return Box::pin(async {
                Err(ProviderError::new(
                    ProviderErrorKind::Transport,
                    "network error during setup",
                ))
            });
        }
        let bug = if self.chunk_alter {
            Bug::Text
        } else {
            self.bug
        };
        let transport: Arc<dyn Transport> = Arc::new(self.transport.clone());
        let cancel = if self.bug == Bug::Cancel {
            CancellationToken::new()
        } else {
            cancel
        };
        let retry = if matches!(self.bug, Bug::Http429 | Bug::Http500) {
            RetryPolicy {
                max_retries: 0,
                ..RetryPolicy::default()
            }
        } else {
            RetryPolicy::default()
        };
        let omit_header = self.bug == Bug::Http401;
        let mut stream = drive(DriveRequest {
            transport,
            credentials: Arc::new(FixedCredential),
            build: Box::new(move |credential| HttpRequest {
                url: "https://reference.invalid/stream".to_string(),
                headers: if omit_header {
                    Vec::new()
                } else {
                    vec![("x-auth".to_string(), credential.bearer.clone())]
                },
                body: Vec::new(),
            }),
            new_parser: Box::new(move || Box::new(LineParser::new(bug))),
            retry,
            cancel,
        });
        if self.bug == Bug::AfterTerminal {
            let altered = async_stream_after_terminal(stream);
            return Box::pin(async move { Ok(altered) });
        }
        if self.bug == Bug::RetryVisible {
            let altered = stream.map(|event| match event {
                StreamEvent::Activity => StreamEvent::TextDelta {
                    block: 0,
                    text: "visible".to_string(),
                },
                other => other,
            });
            stream = Box::pin(altered);
        }
        Box::pin(async move { Ok(stream) })
    }
}

fn async_stream_after_terminal(stream: ProviderStream) -> ProviderStream {
    Box::pin(stream.chain(futures_util::stream::once(async { StreamEvent::Activity })))
}

static CHUNK_BUILDS: AtomicUsize = AtomicUsize::new(0);

fn build_with(transport: ScriptedTransport, bug: Bug) -> Arc<dyn Provider> {
    let chunk_alter = bug == Bug::Chunking && CHUNK_BUILDS.fetch_add(1, Ordering::SeqCst) % 2 == 1;
    Arc::new(ReferenceProvider {
        transport,
        bug,
        chunk_alter,
    })
}

macro_rules! builders {
    ($(($name:ident, $bug:ident)),+ $(,)?) => {$(
        fn $name(transport: ScriptedTransport) -> Arc<dyn Provider> { build_with(transport, Bug::$bug) }
    )+};
}

builders!(
    (build_ok, None),
    (build_text, Text),
    (build_tool, Tool),
    (build_order, Order),
    (build_truncated, Truncated),
    (build_invalid, InvalidJson),
    (build_error, Error),
    (build_usage, Usage),
    (build_replay, Replay),
    (build_after, AfterTerminal),
    (build_chunking, Chunking),
    (build_cancel, Cancel),
    (build_401, Http401),
    (build_429, Http429),
    (build_500, Http500),
    (build_400, Http400),
    (build_retry, RetryVisible),
    (build_setup, Setup),
    (build_accepts_invalid, AcceptsInvalid),
    (build_leak, CredentialLeak),
);

fn build_request(request: &ProviderRequest) -> serde_json::Value {
    let replay: Vec<_> = request
        .history
        .iter()
        .filter_map(|item| match item {
            Item::Assistant(item) if item.origin == origin() => Some(item),
            _ => None,
        })
        .flat_map(|item| item.blocks.iter())
        .filter_map(|block| match block {
            AssistantBlock::Reasoning {
                replay: Some(replay),
                ..
            } if replay.origin == origin() && replay.version == 1 => Some(replay.payload.clone()),
            _ => None,
        })
        .collect();
    serde_json::json!({"prompt": request.system_prompt, "replay": replay})
}

fn invalid_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "conformance prompt".into(),
        history: vec![p1_contracts::Item::User { text: "hi".into() }],
        tools: Vec::new(),
        options: p1_contracts::ModelOptions {
            max_output_tokens: Some(10),
            ..Default::default()
        },
    }
}

fn route(build: fn(ScriptedTransport) -> Arc<dyn Provider>) -> RouteUnderTest {
    RouteUnderTest {
        name: "reference-line",
        build,
        fixtures: FIXTURES,
        follow_up_request: build_request,
        fake_bearer: BEARER,
        invalid_request,
    }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else {
        "non-string panic".to_string()
    }
}

fn caught(
    name: &str,
    check: fn(&RouteUnderTest),
    build: fn(ScriptedTransport) -> Arc<dyn Provider>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&route(build))));
    let message = panic_text(result.expect_err("seeded bug was not caught"));
    assert!(
        message.contains(name),
        "panic did not name {name}: {message}"
    );
}

#[test]
fn correct_reference_route_passes_run_all() {
    run_all(&route(build_ok));
}

#[test]
fn catches_text_terminal_bug() {
    caught(
        "text_deltas_then_single_terminal",
        text_deltas_then_single_terminal,
        build_text,
    );
}
#[test]
fn catches_tool_completion_bug() {
    caught(
        "tool_call_is_complete_and_only_in_terminal",
        tool_call_is_complete_and_only_in_terminal,
        build_tool,
    );
}
#[test]
fn catches_tool_order_bug() {
    caught(
        "tool_call_order_is_preserved",
        tool_call_order_is_preserved,
        build_order,
    );
}
#[test]
fn catches_truncated_completion_bug() {
    caught(
        "truncated_stream_is_failure_not_completion",
        truncated_stream_is_failure_not_completion,
        build_truncated,
    );
}
#[test]
fn catches_invalid_json_repair_bug() {
    caught(
        "invalid_tool_json_is_preserved_raw",
        invalid_tool_json_is_preserved_raw,
        build_invalid,
    );
}
#[test]
fn catches_error_body_leak_bug() {
    caught(
        "error_event_is_single_failed_terminal",
        error_event_is_single_failed_terminal,
        build_error,
    );
}
#[test]
fn catches_unknown_usage_zero_bug() {
    caught(
        "unknown_usage_is_none_not_zero",
        unknown_usage_is_none_not_zero,
        build_usage,
    );
}
#[test]
fn catches_replay_drop_bug() {
    caught(
        "reasoning_replay_round_trips",
        reasoning_replay_round_trips,
        build_replay,
    );
}
#[test]
fn catches_after_terminal_bug() {
    caught(
        "nothing_after_terminal",
        nothing_after_terminal,
        build_after,
    );
}
#[test]
fn catches_chunk_boundary_bug() {
    CHUNK_BUILDS.store(0, Ordering::SeqCst);
    caught(
        "chunking_is_irrelevant",
        chunking_is_irrelevant,
        build_chunking,
    );
}
#[test]
fn catches_cancel_before_first_byte_bug() {
    caught(
        "cancel_before_first_byte",
        cancel_before_first_byte,
        build_cancel,
    );
}
#[test]
fn catches_cancel_mid_stream_bug() {
    caught("cancel_mid_stream", cancel_mid_stream, build_cancel);
}
#[test]
fn catches_401_refresh_bug() {
    caught(
        "http_401_refreshes_once_then_fails_authentication",
        http_401_refreshes_once_then_fails_authentication,
        build_401,
    );
}
#[test]
fn catches_429_retry_bug() {
    caught(
        "http_429_retries_then_succeeds",
        http_429_retries_then_succeeds,
        build_429,
    );
}
#[test]
fn catches_500_budget_bug() {
    caught(
        "http_500_exhausts_budget_then_fails_transport",
        http_500_exhausts_budget_then_fails_transport,
        build_500,
    );
}
#[test]
fn catches_400_retry_classification_bug() {
    caught(
        "http_400_is_invalid_request_without_retry",
        http_400_is_invalid_request_without_retry,
        build_400,
    );
}
#[test]
fn catches_retry_after_output_bug() {
    caught(
        "no_retry_after_visible_output",
        no_retry_after_visible_output,
        build_retry,
    );
}
#[test]
fn catches_setup_error_bug() {
    caught(
        "setup_error_is_only_for_invalid_requests",
        setup_error_is_only_for_invalid_requests,
        build_setup,
    );
}
#[test]
fn catches_a_route_that_accepts_an_invalid_request() {
    caught(
        "setup_error_is_only_for_invalid_requests",
        setup_error_is_only_for_invalid_requests,
        build_accepts_invalid,
    );
}
#[test]
fn catches_credential_leak_bug() {
    caught("credentials_never_leak", credentials_never_leak, build_leak);
}
