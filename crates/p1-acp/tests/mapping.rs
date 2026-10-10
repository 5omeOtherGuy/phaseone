use p1_acp::{
    capabilities::initialize,
    codec::Codec,
    sink::{AcpSink, Outbound, ToolCategory},
    turn::{TurnStop, prompt_outcome},
};
use p1_contracts::*;
use serde_json::{Value, json};
use std::sync::Arc;

fn call(name: &str) -> ToolCall {
    ToolCall {
        call_id: "call-17".into(),
        name: name.into(),
        input: ToolInput::Json(r#"{"path":"a.rs","limit":7}"#.into()),
    }
}

fn update(event: AgentEvent) -> Value {
    let (sink, mut rx) = AcpSink::new();
    sink.emit(event);
    let stamped = rx.try_recv().unwrap();
    assert_eq!(stamped.sequence, 0);
    assert!(rx.try_recv().is_err());
    let Outbound::Update(update) = stamped.item else {
        panic!("expected wire update")
    };
    Codec::negotiate(1).encode_update(&update)
}

#[test]
fn text_delta() {
    assert_eq!(
        update(AgentEvent::TextDelta {
            text: "hello".into()
        }),
        json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"hello"}})
    );
}

#[test]
fn reasoning_delta() {
    assert_eq!(
        update(AgentEvent::ReasoningDelta {
            text: "consider".into()
        }),
        json!({"sessionUpdate":"agent_thought_chunk", "content":{"type":"text","text":"consider"}})
    );
}

#[test]
fn tool_started() {
    let wire = update(AgentEvent::ToolStarted { call: call("read") });
    assert_eq!(wire["sessionUpdate"], "tool_call");
    assert_eq!(wire["toolCallId"], "call-17");
    assert_eq!(wire["title"], "read");
    assert_eq!(wire["kind"], "read");
    assert_eq!(wire["status"], "in_progress");
    assert_eq!(wire["rawInput"], json!({"path":"a.rs","limit":7}));
}

#[test]
fn tool_finished() {
    for status in [
        ToolStatus::Ok,
        ToolStatus::Error,
        ToolStatus::Unavailable,
        ToolStatus::Denied,
        ToolStatus::Cancelled,
        ToolStatus::Unknown,
    ] {
        let wire = update(AgentEvent::ToolFinished {
            result: ToolResultItem {
                call_id: "call-17".into(),
                name: "read".into(),
                status,
                content: "result text".into(),
            },
        });
        assert_eq!(wire["sessionUpdate"], "tool_call_update");
        assert_eq!(wire["toolCallId"], "call-17");
        assert_eq!(
            wire["status"],
            if status == ToolStatus::Ok {
                "completed"
            } else {
                "failed"
            }
        );
        assert_eq!(
            wire["content"],
            json!([{"type":"content","content":{"type":"text","text":"result text"}}])
        );
    }
}

fn ignored(event: AgentEvent) {
    let (sink, mut rx) = AcpSink::new();
    sink.emit(event);
    assert!(rx.try_recv().is_err());
    sink.emit(AgentEvent::TextDelta {
        text: "next".into(),
    });
    assert_eq!(rx.try_recv().unwrap().sequence, 0);
}

#[test]
fn turn_started() {
    ignored(AgentEvent::TurnStarted);
}
#[test]
fn request_started() {
    ignored(AgentEvent::RequestStarted { request_index: 3 });
}
#[test]
fn tool_input_delta() {
    ignored(AgentEvent::ToolInputDelta {
        call_id: "partial".into(),
        name: "read".into(),
        text: "{".into(),
    });
}

fn operator(event: AgentEvent) {
    let (sink, mut rx) = AcpSink::new();
    sink.emit(event.clone());
    assert_eq!(rx.try_recv().unwrap().item, Outbound::Operator(event));
}

#[test]
fn provider_notice() {
    operator(AgentEvent::ProviderNotice {
        text: "retrying".into(),
    });
}
#[test]
fn inbox_delivered() {
    operator(AgentEvent::InboxDelivered { count: 2 });
}
#[test]
fn context_replaced() {
    operator(AgentEvent::ContextReplaced {
        items_before: 9,
        items_after: 4,
        usage: None,
    });
}
#[test]
fn response_completed() {
    ignored(AgentEvent::ResponseCompleted {
        model: "fixture".into(),
        stop: StopReason::ToolUse,
        usage: None,
    });
}

#[test]
fn usage_is_latest_context_and_cumulative_cost_per_session() {
    let (sink, mut rx) = AcpSink::new();
    sink.context_configured(Some(200_000));
    for (input, read, write, output, cost, used, amount) in [
        (41, 70, 13, 900, 1_250, 124, 0.00125),
        (9, 2, 4, 800, 2_000, 15, 0.00325),
    ] {
        sink.emit(AgentEvent::ResponseCompleted {
            model: "fixture".into(),
            stop: StopReason::EndTurn,
            usage: Some(Usage {
                input_uncached: Some(input),
                cache_read: Some(read),
                cache_write: Some(write),
                output: Some(output),
                reasoning_output: Some(300),
                cost_micro_usd: Some(cost),
            }),
        });
        let Outbound::Update(update) = rx.try_recv().unwrap().item else {
            panic!("usage update")
        };
        assert_eq!(
            Codec::negotiate(1).encode_update(&update),
            json!({"sessionUpdate":"usage_update","used":used,"size":200_000,
                "cost":{"amount":amount,"currency":"USD"}})
        );
    }
    let (other, mut other_rx) = AcpSink::new();
    other.context_configured(Some(32_000));
    other.emit(AgentEvent::ResponseCompleted {
        model: "fixture".into(),
        stop: StopReason::EndTurn,
        usage: Some(Usage {
            input_uncached: Some(0),
            cost_micro_usd: Some(0),
            ..Usage::default()
        }),
    });
    let Outbound::Update(update) = other_rx.try_recv().unwrap().item else {
        panic!("usage update")
    };
    assert_eq!(
        Codec::negotiate(1).encode_update(&update),
        json!({"sessionUpdate":"usage_update","used":0,"size":32_000,
            "cost":{"amount":0.0,"currency":"USD"}})
    );
}

#[test]
fn unknown_usage_or_window_sends_nothing_and_unknown_cost_stays_omitted() {
    for window in [None, Some(100_000)] {
        let (sink, mut rx) = AcpSink::new();
        sink.context_configured(window);
        for usage in [None, Some(Usage::default())] {
            sink.emit(AgentEvent::ResponseCompleted {
                model: "fixture".into(),
                stop: StopReason::EndTurn,
                usage,
            });
            assert!(rx.try_recv().is_err());
        }
        for cost in [None, Some(100)] {
            sink.emit(AgentEvent::ResponseCompleted {
                model: "fixture".into(),
                stop: StopReason::EndTurn,
                usage: Some(Usage {
                    input_uncached: Some(17),
                    cost_micro_usd: cost,
                    ..Usage::default()
                }),
            });
            if window.is_none() {
                assert!(rx.try_recv().is_err());
            } else {
                let Outbound::Update(update) = rx.try_recv().unwrap().item else {
                    panic!("usage update")
                };
                assert_eq!(
                    Codec::negotiate(1).encode_update(&update),
                    json!({"sessionUpdate":"usage_update","used":17,"size":100_000})
                );
            }
        }
    }
}

#[test]
fn turn_finished() {
    let (sink, mut rx) = AcpSink::new();
    sink.emit(AgentEvent::TurnFinished {
        end: TurnEnd::Cancelled,
    });
    let Outbound::Turn(Ok(response)) = rx.try_recv().unwrap().item else {
        panic!("expected outcome")
    };
    assert_eq!(response, TurnStop::Cancelled);
}

macro_rules! stop_test {
    ($test:ident, $p1:ident, $wire:literal) => {
        #[test]
        fn $test() {
            let response = prompt_outcome(TurnEnd::Completed { stop: StopReason::$p1 }).unwrap();
            assert_eq!(Codec::negotiate(1).encode_stop(response), json!({"stopReason":$wire}));
        }
    };
}
stop_test!(end_turn, EndTurn, "end_turn");
stop_test!(tool_use, ToolUse, "end_turn");
stop_test!(max_output_tokens, MaxOutputTokens, "max_tokens");
stop_test!(context_window_exceeded, ContextWindowExceeded, "max_tokens");
stop_test!(refusal, Refusal, "refusal");
stop_test!(paused, Paused, "end_turn");
stop_test!(other, Other, "end_turn");

#[test]
fn cancelled() {
    assert_eq!(
        Codec::negotiate(1).encode_stop(prompt_outcome(TurnEnd::Cancelled).unwrap()),
        json!({"stopReason":"cancelled"})
    );
}
fn failure(end: TurnEnd, message: &str) {
    assert_eq!(
        Codec::negotiate(1).encode_error(&prompt_outcome(end).unwrap_err()),
        json!({"code":-32603,"message":message})
    );
}
#[test]
fn provider_failed() {
    failure(
        TurnEnd::ProviderFailed {
            error: ProviderError::new(ProviderErrorKind::Transport, "connection closed"),
        },
        "Transport: connection closed",
    );
}
#[test]
fn commit_failed() {
    failure(
        TurnEnd::CommitFailed {
            message: "journal refused".into(),
        },
        "journal refused",
    );
}
#[test]
fn context_failed() {
    failure(
        TurnEnd::ContextFailed {
            message: "preparation failed".into(),
        },
        "preparation failed",
    );
}

#[test]
fn capabilities_round_trip() {
    let client = json!({"p1.dev":{"version":1,"capabilities":["future"]}});
    let (codec, capabilities) = initialize(1, client.as_object());
    let wire = codec.encode_capabilities(&capabilities);
    assert_eq!(wire["protocolVersion"], 1);
    assert_eq!(wire["authMethods"], json!([]));
    assert_eq!(wire["agentCapabilities"]["loadSession"], false);
    assert_eq!(
        wire["agentCapabilities"]["promptCapabilities"],
        json!({"image":false,"audio":false,"embeddedContext":false})
    );
    assert_eq!(
        wire["agentCapabilities"]["_meta"]["p1.dev"],
        json!({"version":1,"extensions":[]})
    );
    assert_eq!(
        wire,
        json!({"protocolVersion":1,"authMethods":[],"agentInfo":{"name":"p1","version":env!("CARGO_PKG_VERSION")},"agentCapabilities":{
            "loadSession":false,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},
            "_meta":{"p1.dev":{"version":1,"extensions":[]}}
        }})
    );
}
#[test]
fn non_declaring_client_gets_no_key() {
    let (codec, capabilities) = initialize(1, None);
    let wire = codec.encode_capabilities(&capabilities);
    assert!(wire["agentCapabilities"].get("_meta").is_none());
}
#[test]
fn malformed_declaration_enables_nothing() {
    for declaration in [
        Value::Null,
        json!(true),
        json!({}),
        json!({"version":1}),
        json!({"version":"1","capabilities":[]}),
        json!({"version":-1,"capabilities":[]}),
        json!({"version":1,"capabilities":[4]}),
        json!({"version":1,"capabilities":{}}),
        json!({"version":2,"capabilities":[]}),
    ] {
        let client = json!({"p1.dev":declaration});
        assert_eq!(initialize(1, client.as_object()), initialize(1, None));
    }
}

struct DescribedTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}
impl Tool for DescribedTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _: &ToolCall) -> Effect {
        Effect::ReadOnly
    }
    fn describe(&self, _: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "search",
            target: Some("tool-owned target".into()),
            edit: None,
            destructive: false,
        }
    }
    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async { panic!("mapping never executes tools") })
    }
}
#[test]
fn announced_tools_own_title_and_kind() {
    let (sink, mut rx) = AcpSink::new();
    sink.parent_tools(&[Arc::new(DescribedTool {
        declaration: ToolDeclaration {
            name: "alias".into(),
            description: String::new(),
            kind: DeclarationKind::Function {
                input_schema: json!({}),
            },
        },
        identity: ToolIdentity {
            implementation: "fixture".into(),
            variant: "default".into(),
        },
    })]);
    sink.emit(AgentEvent::ToolStarted {
        call: call("alias"),
    });
    let Outbound::Update(wire) = rx.try_recv().unwrap().item else {
        panic!("update")
    };
    let wire = Codec::negotiate(1).encode_update(&wire);
    assert_eq!(wire["title"], "search tool-owned target");
    assert_eq!(wire["kind"], "search");
    sink.parent_tools(&[]);
    assert_eq!(sink.describe_call(&call("alias")).title, "alias");
}
#[test]
fn tool_kind_table_and_raw_inputs() {
    let (sink, _) = AcpSink::new();
    for (name, kind) in [
        ("read", ToolCategory::Read),
        ("edit", ToolCategory::Edit),
        ("delete", ToolCategory::Delete),
        ("move", ToolCategory::Move),
        ("search", ToolCategory::Search),
        ("execute", ToolCategory::Execute),
        ("think", ToolCategory::Think),
        ("fetch", ToolCategory::Fetch),
        ("unknown", ToolCategory::Other),
    ] {
        assert_eq!(sink.describe_call(&call(name)).category, kind);
    }
    for input in [
        ToolInput::Text("{not json}".into()),
        ToolInput::Json("{not json}".into()),
    ] {
        let mut call = call("custom");
        call.input = input;
        assert_eq!(sink.describe_call(&call).input, json!("{not json}"));
    }
}
#[test]
fn sequence_orders_all_outbound_items_under_concurrent_emitters() {
    let (sink, mut rx) = AcpSink::new();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..50 {
                    sink.emit(AgentEvent::InboxDelivered { count: 1 });
                }
            });
        }
    });
    for sequence in 0..200 {
        assert_eq!(rx.try_recv().unwrap().sequence, sequence);
    }
    assert!(rx.try_recv().is_err());
}
