use p1_acp::{
    capabilities::initialize,
    codec::Codec,
    sink::{AcpSink, Outbound, ToolCategory},
    turn::{TurnStop, prompt_outcome},
};
use p1_contracts::frontend::WorkflowStep;
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
fn plan_snapshots_keep_run_order_ordinals_and_task_text() {
    let (sink, mut rx) = AcpSink::new();
    sink.workflow_started("wf2");
    sink.workflow_started("wf10");
    let mut next = || {
        let Outbound::Update(update) = rx.try_recv().unwrap().item else {
            panic!("plan update")
        };
        Codec::negotiate(1).encode_update(&update)
    };
    let mut step = WorkflowStep {
        run: "wf10".into(),
        ordinal: 1,
        call: "same".into(),
        label: Some("Later run".into()),
        task: Some("label wins".into()),
        status: "running".into(),
    };
    sink.workflow_step(&step);
    assert_eq!(
        next(),
        json!({"sessionUpdate":"plan","entries":[
            {"content":"wf10/1: Later run","priority":"medium","status":"in_progress"}
        ]})
    );
    step.run = "wf2".into();
    step.ordinal = 3;
    step.label = None;
    step.task = Some("Write task".into());
    sink.workflow_step(&step);
    assert_eq!(
        next(),
        json!({"sessionUpdate":"plan","entries":[
            {"content":"wf2/3: Write task","priority":"medium","status":"in_progress"},
            {"content":"wf10/1: Later run","priority":"medium","status":"in_progress"}
        ]})
    );
    step.ordinal = 1;
    step.label = Some("Earlier step".into());
    sink.workflow_step(&step);
    let three = json!({"sessionUpdate":"plan","entries":[
        {"content":"wf2/1: Earlier step","priority":"medium","status":"in_progress"},
        {"content":"wf2/3: Write task","priority":"medium","status":"in_progress"},
        {"content":"wf10/1: Later run","priority":"medium","status":"in_progress"}
    ]});
    assert_eq!(next(), three);
    // Repeated starts (including fallbacks) emit a full snapshot, not another row.
    sink.workflow_step(&step);
    assert_eq!(next(), three);
    step.ordinal = 3;
    step.label = None;
    step.task = None;
    step.status = "done".into();
    sink.workflow_step(&step);
    assert_eq!(
        next(),
        json!({"sessionUpdate":"plan","entries":[
            {"content":"wf2/1: Earlier step","priority":"medium","status":"in_progress"},
            {"content":"wf2/3: Write task","priority":"medium","status":"completed"},
            {"content":"wf10/1: Later run","priority":"medium","status":"in_progress"}
        ]})
    );
}

#[test]
fn unfinished_plan_outcomes_are_pending_explicit_and_session_local() {
    let (sink, mut rx) = AcpSink::new();
    for (ordinal, status) in [(1, "failed"), (2, "blocked"), (3, "cancelled")] {
        sink.workflow_step(&WorkflowStep {
            run: "wf1".into(),
            ordinal,
            call: "call-only".into(),
            label: None,
            task: None,
            status: status.into(),
        });
    }
    let mut last = Value::Null;
    while let Ok(stamped) = rx.try_recv() {
        let Outbound::Update(update) = stamped.item else {
            panic!("plan update")
        };
        last = Codec::negotiate(1).encode_update(&update);
    }
    assert_eq!(
        last,
        json!({"sessionUpdate":"plan","entries":[
            {"content":"wf1/1: call-only (failed)","priority":"medium","status":"pending"},
            {"content":"wf1/2: call-only (blocked)","priority":"medium","status":"pending"},
            {"content":"wf1/3: call-only (cancelled)","priority":"medium","status":"pending"}
        ]})
    );
    let (other, mut other_rx) = AcpSink::new();
    other.workflow_step(&WorkflowStep {
        run: "wf1".into(),
        ordinal: 1,
        call: "call-only".into(),
        label: Some("Replayed".into()),
        task: None,
        status: "done".into(),
    });
    let Outbound::Update(update) = other_rx.try_recv().unwrap().item else {
        panic!("plan update")
    };
    assert_eq!(
        Codec::negotiate(1).encode_update(&update),
        json!({"sessionUpdate":"plan","entries":[
            {"content":"wf1/1: Replayed","priority":"medium","status":"completed"}
        ]})
    );
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

#[test]
fn workflow_cards_buffer_early_progress_and_close_with_the_run_outcome() {
    use p1_contracts::frontend::WorkflowProgress;
    for (outcome, expected_status) in [
        ("completed", "completed"),
        ("completed_with_issues", "failed"),
        ("failed", "failed"),
        ("cancelled", "failed"),
    ] {
        for early in [false, true] {
            let (sink, mut rx) = AcpSink::new();
            sink.parent_tools(&[Arc::new(DescribedTool {
                declaration: ToolDeclaration {
                    name: "run-script".into(),
                    description: String::new(),
                    kind: DeclarationKind::Function {
                        input_schema: json!({}),
                    },
                },
                identity: ToolIdentity {
                    implementation: "p1/workflow-start".into(),
                    variant: "default".into(),
                },
            })]);
            sink.emit(AgentEvent::ToolStarted {
                call: call("run-script"),
            });
            rx.try_recv().unwrap();
            let progress = WorkflowProgress {
                run: "wf2".into(),
                line: "workflow wf2: preparing".into(),
                outcome: None,
            };
            sink.workflow_progress(&WorkflowProgress {
                run: "wf9".into(),
                line: "unrelated run".into(),
                outcome: None,
            });
            let ended = WorkflowProgress {
                run: "wf2".into(),
                line: format!("workflow wf2 {outcome}"),
                outcome: Some(outcome.into()),
            };
            if early {
                sink.workflow_progress(&progress);
                sink.workflow_progress(&ended);
            }
            sink.emit(AgentEvent::ToolFinished { result: ToolResultItem {
                call_id: "call-17".into(), name: "run-script".into(), status: ToolStatus::Ok,
                content: "Started workflow wf2, resuming wf1. You will be notified when it ends; do not poll.".into(),
            }});
            if !early {
                sink.workflow_progress(&progress);
                sink.workflow_progress(&ended);
            }
            let mut frames = Vec::new();
            while let Ok(stamped) = rx.try_recv() {
                let Outbound::Update(update) = stamped.item else {
                    panic!("card update")
                };
                frames.push(Codec::negotiate(1).encode_update(&update));
            }
            assert_eq!(frames.len(), 3);
            let start = "Started workflow wf2, resuming wf1. You will be notified when it ends; do not poll.";
            for (frame, (status, text)) in frames.iter().zip([
                ("in_progress", start.to_string()),
                ("in_progress", format!("{start}\nworkflow wf2: preparing")),
                (
                    expected_status,
                    format!("{start}\nworkflow wf2: preparing\nworkflow wf2 {outcome}"),
                ),
            ]) {
                assert_eq!(
                    *frame,
                    json!({"sessionUpdate":"tool_call_update", "toolCallId":"call-17",
                    "status":status,"content":[{"type":"content","content":{"type":"text","text":text}}]})
                );
            }
        }
    }
}

#[test]
fn worker_notes_append_without_reopening_and_never_rewrite_reused_calls() {
    let (sink, mut rx) = AcpSink::new();
    sink.emit(AgentEvent::ToolStarted {
        call: call("worker_start"),
    });
    rx.try_recv().unwrap();
    sink.worker_ended("w42", "worker w42 done: verified");
    sink.worker_ended("w7", "unlinked worker note");
    assert!(rx.try_recv().is_err());
    sink.emit(AgentEvent::ToolFinished {
        result: ToolResultItem {
            call_id: "call-17".into(),
            name: "worker_start".into(),
            status: ToolStatus::Ok,
            content: "Started worker w42 on fake/main".into(),
        },
    });
    let mut frames = Vec::new();
    while let Ok(stamped) = rx.try_recv() {
        let Outbound::Update(update) = stamped.item else {
            panic!("worker update")
        };
        frames.push(Codec::negotiate(1).encode_update(&update));
    }
    assert_eq!(frames[0]["status"], "completed");
    assert_eq!(
        frames[1],
        json!({"sessionUpdate":"tool_call_update","toolCallId":"call-17",
        "content":[{"type":"content","content":{"type":"text",
            "text":"Started worker w42 on fake/main\nworker w42 done: verified"}}]})
    );
    assert_eq!(
        frames[2],
        json!({"sessionUpdate":"agent_message_chunk",
        "content":{"type":"text","text":"unlinked worker note"}})
    );
    assert_eq!(frames.len(), 3);
    sink.emit(AgentEvent::ToolStarted { call: call("read") });
    rx.try_recv().unwrap();
    sink.worker_ended("w42", "late old worker note");
    sink.emit(AgentEvent::ToolFinished {
        result: ToolResultItem {
            call_id: "call-17".into(),
            name: "read".into(),
            status: ToolStatus::Ok,
            content: "Started worker w42 on fake/main".into(),
        },
    });
    rx.try_recv().unwrap();
    let Outbound::Update(update) = rx.try_recv().unwrap().item else {
        panic!("orphan note")
    };
    assert_eq!(
        Codec::negotiate(1).encode_update(&update),
        json!({"sessionUpdate":"agent_message_chunk",
        "content":{"type":"text","text":"late old worker note"}})
    );
    let (other, mut other_rx) = AcpSink::new();
    other.worker_ended("w42", "independent session");
    let Outbound::Update(update) = other_rx.try_recv().unwrap().item else {
        panic!("session note")
    };
    assert_eq!(
        Codec::negotiate(1).encode_update(&update)["sessionUpdate"],
        "agent_message_chunk"
    );
}

#[test]
fn authored_plan_replaces_instead_of_merging_and_workflows_cannot_overwrite_it() {
    use p1_contracts::plan::{PlanEntry, PlanPriority, PlanStatus};
    let (sink, mut rx) = AcpSink::new();
    let authored = vec![PlanEntry {
        content: "Verify asymmetric case".into(),
        status: PlanStatus::InProgress,
        priority: PlanPriority::High,
    }];
    let expected = json!({"sessionUpdate":"plan","entries":[{"content":"Verify asymmetric case","status":"in_progress","priority":"high"}]});
    sink.plan_updated(&authored);
    sink.workflow_step(&WorkflowStep {
        run: "wf1".into(),
        ordinal: 1,
        call: "c1".into(),
        label: Some("Different workflow task".into()),
        task: None,
        status: "done".into(),
    });
    for _ in 0..2 {
        let Outbound::Update(update) = rx.try_recv().unwrap().item else {
            panic!("plan")
        };
        assert_eq!(Codec::negotiate(1).encode_update(&update), expected);
    }
    sink.plan_updated(&[]);
    let Outbound::Update(update) = rx.try_recv().unwrap().item else {
        panic!("plan")
    };
    assert_eq!(
        Codec::negotiate(1).encode_update(&update),
        json!({"sessionUpdate":"plan","entries":[]})
    );
    let (other, mut other_rx) = AcpSink::new();
    other.plan_updated(&[PlanEntry {
        content: "Independent".into(),
        status: PlanStatus::Completed,
        priority: PlanPriority::Low,
    }]);
    let Outbound::Update(update) = other_rx.try_recv().unwrap().item else {
        panic!("plan")
    };
    assert_eq!(
        Codec::negotiate(1).encode_update(&update)["entries"][0]["content"],
        "Independent"
    );
    assert!(rx.try_recv().is_err());
}
