use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    AgentEvent, AssistantBlock, CancellationToken, Effect, InboxKind, InterruptionReason, Item,
    JournalRecord, ModelOptions, Outcome, Provider, ProviderError, ProviderErrorKind, RecordBody,
    StopReason, StreamEvent, Tool, ToolResultItem, ToolStatus, TurnEnd, Usage,
};
use p1_core::{Agent, AgentParts, BuildError, Inbox};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ReplacingContext,
    ScriptedAuthorization, ScriptedProvider, Step, completed, json_call, origin, text_block,
    text_response, tool_call_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

struct Fixture {
    provider: Arc<ScriptedProvider>,
    journal: Arc<RecordingJournal>,
    events: Arc<RecordingEvents>,
    authorization: Arc<ScriptedAuthorization>,
}

fn options() -> ModelOptions {
    ModelOptions {
        reasoning_effort: Some(p1_contracts::Effort::High),
        max_output_tokens: Some(321),
        cache_key: Some("acceptance-cache".into()),
        native: [("fake-route.option".into(), serde_json::json!("value"))]
            .into_iter()
            .collect(),
    }
}

fn parts(
    provider: Arc<ScriptedProvider>,
    tools: Vec<Arc<dyn p1_contracts::Tool>>,
) -> (AgentParts, Fixture) {
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let authorization = Arc::new(ScriptedAuthorization::permit_all());
    (
        AgentParts {
            provider: provider.clone(),
            tools,
            system_prompt: "system acceptance prompt".into(),
            options: options(),
            context: Arc::new(PassthroughContext),
            authorization: authorization.clone(),
            journal: journal.clone(),
            events: events.clone(),
        },
        Fixture {
            provider,
            journal,
            events,
            authorization,
        },
    )
}

fn agent_with(script: Vec<Step>) -> (Agent, Fixture) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let (parts, fixture) = parts(provider, vec![]);
    (Agent::new(parts).unwrap(), fixture)
}

async fn run(agent: &mut Agent, input: &str, cancel: CancellationToken) -> TurnEnd {
    timeout(LIMIT, agent.run_turn(input.into(), cancel))
        .await
        .expect("run_turn timed out")
}

fn record(seq: u64, body: RecordBody) -> JournalRecord {
    JournalRecord { seq, body }
}

fn user(text: &str) -> Item {
    Item::User { text: text.into() }
}

fn tool_result(call_id: &str, name: &str, status: ToolStatus, content: &str) -> ToolResultItem {
    ToolResultItem {
        call_id: call_id.into(),
        name: name.into(),
        status,
        content: content.into(),
    }
}

fn assert_last_finished(events: &[AgentEvent], end: &TurnEnd) {
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnFinished { end: end.clone() })
    );
}

#[tokio::test(start_paused = true)]
async fn construction_rejects_duplicate_tool_names() {
    // Spec §1: duplicate assembled call names are rejected.
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let one = Arc::new(FakeTool::new("same"));
    let two = Arc::new(FakeTool::new("same"));
    let (parts, fixture) = parts(provider, vec![one, two]);
    assert!(matches!(
        Agent::new(parts),
        Err(BuildError::DuplicateToolName(name)) if name == "same"
    ));
    assert!(fixture.journal.records().is_empty());
    assert!(fixture.events.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn construction_returns_provider_validation_rejection_without_side_effects() {
    // Spec §1: provider validation failure is a build error and construction is silent.
    let error = ProviderError::new(ProviderErrorKind::InvalidRequest, "unsupported option");
    let provider = Arc::new(ScriptedProvider::new(vec![]).rejecting_validation(error.clone()));
    let (parts, fixture) = parts(provider, vec![Arc::new(FakeTool::new("read"))]);
    assert_eq!(
        Agent::new(parts).err(),
        Some(BuildError::ProviderRejected(error))
    );
    assert!(fixture.journal.records().is_empty());
    assert!(fixture.events.events().is_empty());
    assert!(fixture.provider.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn successful_construction_commits_and_emits_nothing() {
    // Spec §1: successful construction also has no journal or event side effects.
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let (parts, fixture) = parts(provider, vec![Arc::new(FakeTool::new("read"))]);
    let _agent = Agent::new(parts).unwrap();
    assert!(fixture.journal.records().is_empty());
    assert!(fixture.events.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn environment_is_exactly_once_and_sequences_are_dense_across_two_turns() {
    // Spec §2: environment is seq 0 once, precedes input, and seq is dense across turns.
    let first = text_response("first");
    let second = text_response("second");
    let provider = Arc::new(ScriptedProvider::new(vec![first.clone(), second.clone()]));
    let alpha = Arc::new(FakeTool::new("alpha").with_identity("impl-a", "variant-a"));
    let beta = Arc::new(FakeTool::new("beta").with_identity("impl-b", "variant-b"));
    let expected_tools = vec![
        (alpha.declaration().clone(), alpha.identity().clone()),
        (beta.declaration().clone(), beta.identity().clone()),
    ];
    let (parts, fixture) = parts(provider, vec![alpha, beta]);
    let mut agent = Agent::new(parts).unwrap();

    assert_eq!(
        run(&mut agent, "one", CancellationToken::new()).await,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(
        run(&mut agent, "two", CancellationToken::new()).await,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let records = fixture.journal.records();
    assert_eq!(
        records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert_eq!(
        records[0],
        record(
            0,
            RecordBody::Environment {
                route: fixture.provider.describe(),
                system_prompt: "system acceptance prompt".into(),
                tools: expected_tools,
                options: options(),
            }
        )
    );
    assert_eq!(
        records[1],
        record(1, RecordBody::UserInput { text: "one".into() })
    );
    assert!(matches!(
        records[2].body,
        RecordBody::AssistantCompleted { .. }
    ));
    assert_eq!(
        records[3],
        record(3, RecordBody::UserInput { text: "two".into() })
    );
    assert!(matches!(
        records[4].body,
        RecordBody::AssistantCompleted { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn plain_text_turn_has_exact_records_and_events_and_preserves_unknown_usage() {
    // Spec §3: a plain response has the exact observable record/event sequence.
    let response = completed(vec![text_block("hello")], StopReason::EndTurn, None);
    let (mut agent, fixture) = agent_with(vec![Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "hel".into(),
        },
        StreamEvent::TextDelta {
            block: 0,
            text: "lo".into(),
        },
        StreamEvent::Finished(response.clone()),
    ])]);
    let end = run(&mut agent, "question", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    let Outcome::Completed(done) = response else {
        unreachable!()
    };
    assert_eq!(
        fixture.journal.records(),
        vec![
            record(
                0,
                RecordBody::Environment {
                    route: fixture.provider.describe(),
                    system_prompt: "system acceptance prompt".into(),
                    tools: vec![],
                    options: options(),
                }
            ),
            record(
                1,
                RecordBody::UserInput {
                    text: "question".into()
                }
            ),
            record(
                2,
                RecordBody::AssistantCompleted {
                    item: done.item.clone(),
                    stop: StopReason::EndTurn,
                    usage: None
                }
            ),
        ]
    );
    assert_eq!(
        fixture.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta { text: "hel".into() },
            AgentEvent::TextDelta { text: "lo".into() },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None
            },
            AgentEvent::TurnFinished { end: end.clone() },
        ]
    );
    assert_eq!(
        agent.history(),
        &[user("question"), Item::Assistant(done.item)]
    );
}

#[tokio::test(start_paused = true)]
async fn reasoning_is_emitted_and_activity_is_not_observable() {
    // Spec §3d: reasoning is display-only and Activity has no observation.
    let (mut agent, fixture) = agent_with(vec![Step::Events(vec![
        StreamEvent::ReasoningDelta {
            block: 0,
            text: "thinking".into(),
        },
        StreamEvent::Activity,
        StreamEvent::Finished(completed(
            vec![text_block("answer")],
            StopReason::EndTurn,
            None,
        )),
    ])]);
    run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        fixture.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ReasoningDelta {
                text: "thinking".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_notice_is_forwarded_in_order_and_is_nothing_else() {
    // ADR-0048: a notice is display-only. It becomes `ProviderNotice` where it
    // arrived, and it is neither history, nor a journal record, nor partial text.
    let notice = "transport: WebSocket unavailable (HTTP 501) — using HTTP (SSE) for the rest of this session";
    let (mut agent, fixture) = agent_with(vec![Step::Events(vec![
        StreamEvent::Notice {
            text: notice.into(),
        },
        StreamEvent::TextDelta {
            block: 0,
            text: "answer".into(),
        },
        StreamEvent::Notice {
            text: "second notice".into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block("answer")],
            StopReason::EndTurn,
            None,
        )),
    ])]);
    run(&mut agent, "q", CancellationToken::new()).await;

    assert_eq!(
        fixture.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ProviderNotice {
                text: notice.into()
            },
            AgentEvent::TextDelta {
                text: "answer".into()
            },
            AgentEvent::ProviderNotice {
                text: "second notice".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    // Exactly the three records of a plain text turn: no notice record, and the
    // notice text is not part of the completed item either.
    let records = fixture.journal.records();
    assert_eq!(records.len(), 3);
    assert!(matches!(
        records[2].body,
        RecordBody::AssistantCompleted { .. }
    ));
    assert_eq!(agent.history().len(), 2, "no notice entered the history");
}

#[tokio::test(start_paused = true)]
async fn response_usage_is_passed_through_unchanged() {
    // Spec §3e: known usage is passed unchanged to the record and event.
    let usage = Usage {
        input_uncached: Some(7),
        cache_read: Some(2),
        cache_write: Some(3),
        output: Some(5),
        reasoning_output: Some(1),
        cost_micro_usd: Some(99),
    };
    let done = completed(
        vec![text_block("answer")],
        StopReason::MaxOutputTokens,
        Some(usage),
    );
    let (mut agent, fixture) = agent_with(vec![Step::Events(vec![StreamEvent::Finished(done)])]);
    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert!(
        matches!(fixture.journal.records()[2].body, RecordBody::AssistantCompleted { usage: Some(value), .. } if value == usage)
    );
    assert!(
        fixture
            .events
            .events()
            .contains(&AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::MaxOutputTokens,
                usage: Some(usage)
            })
    );
    assert_last_finished(&fixture.events.events(), &end);
}

#[tokio::test(start_paused = true)]
async fn requests_contain_prompt_ordered_tools_options_and_full_prior_history() {
    // Spec §3 request content: every request receives the exact assembled environment and history.
    let call = json_call("c1", "alpha", "{}");
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![call.clone()]),
        text_response("after tool"),
        text_response("next turn"),
    ]));
    let alpha = Arc::new(FakeTool::new("alpha"));
    let beta = Arc::new(FakeTool::new("beta"));
    let declarations = vec![alpha.declaration().clone(), beta.declaration().clone()];
    let (parts, fixture) = parts(provider, vec![alpha, beta]);
    let mut agent = Agent::new(parts).unwrap();
    run(&mut agent, "first user", CancellationToken::new()).await;
    run(&mut agent, "second user", CancellationToken::new()).await;
    let requests = fixture.provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.system_prompt, "system acceptance prompt");
        assert_eq!(request.tools, declarations);
        assert_eq!(request.options, options());
    }
    assert_eq!(requests[0].history, vec![user("first user")]);
    let first_assistant = match &fixture.journal.records()[2].body {
        RecordBody::AssistantCompleted { item, .. } => item.clone(),
        _ => panic!("wrong record"),
    };
    let result = tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok");
    assert_eq!(
        requests[1].history,
        vec![
            user("first user"),
            Item::Assistant(first_assistant.clone()),
            Item::ToolResult(result.clone())
        ]
    );
    let second_assistant = match &fixture.journal.records()[5].body {
        RecordBody::AssistantCompleted { item, .. } => item.clone(),
        _ => panic!("wrong record"),
    };
    assert_eq!(
        requests[2].history,
        vec![
            user("first user"),
            Item::Assistant(first_assistant),
            Item::ToolResult(result),
            Item::Assistant(second_assistant),
            user("second user")
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn provider_setup_error_records_exact_interruption() {
    // Spec §3c: stream setup failure records a blank interrupted response.
    let error = ProviderError::new(ProviderErrorKind::Authentication, "no access");
    let (mut agent, fixture) = agent_with(vec![Step::SetupError(error.clone())]);
    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::ProviderFailed {
            error: error.clone()
        }
    );
    assert_eq!(
        fixture.journal.records()[2],
        record(
            2,
            RecordBody::AssistantInterrupted {
                reason: InterruptionReason::ProviderFailed,
                partial_text: "".into(),
                error: Some(error)
            }
        )
    );
    assert_eq!(agent.history(), &[user("q")]);
    assert_last_finished(&fixture.events.events(), &end);
}

#[tokio::test(start_paused = true)]
async fn failed_terminal_records_partial_text_and_adds_no_response_to_history() {
    // Spec §3d: Finished(Failed) retains streamed partial text only in the interruption record.
    let error = ProviderError::new(ProviderErrorKind::RateLimited, "retry later");
    let (mut agent, fixture) = agent_with(vec![Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "partial ".into(),
        },
        StreamEvent::TextDelta {
            block: 0,
            text: "answer".into(),
        },
        StreamEvent::Finished(Outcome::Failed(error.clone())),
    ])]);
    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::ProviderFailed {
            error: error.clone()
        }
    );
    assert_eq!(
        fixture.journal.records()[2],
        record(
            2,
            RecordBody::AssistantInterrupted {
                reason: InterruptionReason::ProviderFailed,
                partial_text: "partial answer".into(),
                error: Some(error)
            }
        )
    );
    assert_eq!(agent.history(), &[user("q")]);
    assert_last_finished(&fixture.events.events(), &end);
}

#[tokio::test(start_paused = true)]
async fn stream_eof_is_the_exact_transport_failure() {
    // Spec §3d: EOF without Finished is a precisely worded transport failure.
    let (mut agent, fixture) = agent_with(vec![Step::Events(vec![])]);
    let error = ProviderError::new(
        ProviderErrorKind::Transport,
        "stream ended without a terminal event",
    );
    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::ProviderFailed {
            error: error.clone()
        }
    );
    assert_eq!(
        fixture.journal.records()[2],
        record(
            2,
            RecordBody::AssistantInterrupted {
                reason: InterruptionReason::ProviderFailed,
                partial_text: "".into(),
                error: Some(error)
            }
        )
    );
    assert_last_finished(&fixture.events.events(), &end);
}

#[tokio::test(start_paused = true)]
async fn events_after_finished_are_not_observed() {
    // Spec §3d: the stream is dropped at Finished and later events are never read.
    let (mut agent, fixture) = agent_with(vec![Step::Events(vec![
        StreamEvent::Finished(completed(
            vec![text_block("done")],
            StopReason::EndTurn,
            None,
        )),
        StreamEvent::TextDelta {
            block: 0,
            text: "forbidden".into(),
        },
    ])]);
    run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        fixture.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn tool_input_delta_is_display_only() {
    // Spec §3d: partial tool input is emitted but neither stored nor executed.
    let tool = Arc::new(FakeTool::new("alpha"));
    let provider = Arc::new(ScriptedProvider::new(vec![Step::Events(vec![
        StreamEvent::ToolInputDelta {
            call_id: "draft-call".into(),
            text: "{\"x\":".into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block("no call")],
            StopReason::EndTurn,
            None,
        )),
    ])]));
    let (parts, fixture) = parts(provider, vec![tool.clone()]);
    let mut agent = Agent::new(parts).unwrap();
    run(&mut agent, "q", CancellationToken::new()).await;
    assert!(
        fixture
            .events
            .events()
            .contains(&AgentEvent::ToolInputDelta {
                call_id: "draft-call".into(),
                text: "{\"x\":".into()
            })
    );
    assert!(tool.calls().is_empty());
    assert!(!format!("{:?}", fixture.journal.records()).contains("draft-call"));
}

#[tokio::test(start_paused = true)]
async fn paused_response_re_requests_with_same_history_and_increments_request_index() {
    // Spec §3f: Paused starts another request with unchanged history.
    let paused = completed(vec![text_block("part")], StopReason::Paused, None);
    let (mut agent, fixture) = agent_with(vec![
        Step::Events(vec![StreamEvent::Finished(paused)]),
        text_response("done"),
    ]);
    run(&mut agent, "q", CancellationToken::new()).await;
    let requests = fixture.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].history, agent.history()[..2]);
    assert_eq!(
        fixture
            .events
            .events()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::RequestStarted { request_index } => Some(*request_index),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test(start_paused = true)]
async fn passthrough_context_sends_history_unchanged() {
    // Spec §3b: Ok(None) preserves the current history.
    let (mut agent, fixture) = agent_with(vec![text_response("answer")]);
    run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(fixture.provider.requests()[0].history, vec![user("q")]);
    assert!(
        !fixture
            .journal
            .records()
            .iter()
            .any(|r| matches!(r.body, RecordBody::ContextReplaced { .. }))
    );
}

#[tokio::test(start_paused = true)]
async fn context_replacement_is_journalled_and_is_the_history_sent() {
    // Spec §3b: Ok(Some) commits and installs the replacement before requesting.
    let replacement = vec![user("compressed")];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("answer")]));
    let (mut parts, fixture) = parts(provider, vec![]);
    parts.context = Arc::new(ReplacingContext::new(0, replacement.clone()));
    let mut agent = Agent::new(parts).unwrap();
    run(&mut agent, "long input", CancellationToken::new()).await;
    assert_eq!(
        fixture.journal.records()[2],
        record(
            2,
            RecordBody::ContextReplaced {
                items: replacement.clone(),
                usage: None,
            }
        )
    );
    assert_eq!(fixture.provider.requests()[0].history, replacement);
}

#[tokio::test(start_paused = true)]
async fn context_failure_ends_with_exact_message_before_provider_request() {
    // Spec §3b: context failure ends the turn with ContextFailed.
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let (mut parts, fixture) = parts(provider, vec![]);
    parts.context = Arc::new(ReplacingContext::failing());
    let mut agent = Agent::new(parts).unwrap();
    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::ContextFailed {
            message: "scripted context failure".into()
        }
    );
    assert!(fixture.provider.requests().is_empty());
    assert_last_finished(&fixture.events.events(), &end);
}

#[tokio::test(start_paused = true)]
async fn unavailable_tool_has_exact_result_and_is_not_authorized() {
    // Spec §4: an unassembled tool is unavailable with exact model-visible text.
    let call = json_call("missing-id", "missing", "{}");
    let (mut agent, fixture) =
        agent_with(vec![tool_call_response(vec![call]), text_response("done")]);
    run(&mut agent, "q", CancellationToken::new()).await;
    let result = tool_result(
        "missing-id",
        "missing",
        ToolStatus::Unavailable,
        "Tool `missing` is not available.",
    );
    assert_eq!(
        fixture.journal.records()[3],
        record(
            3,
            RecordBody::ToolFinished {
                result: result.clone()
            }
        )
    );
    assert!(fixture.events.events().contains(&AgentEvent::ToolFinished {
        result: result.clone()
    }));
    assert!(agent.history().contains(&Item::ToolResult(result)));
    assert!(fixture.authorization.seen().is_empty());
}

#[tokio::test(start_paused = true)]
async fn denied_tool_has_reason_and_neither_starts_nor_executes() {
    // Spec §4: denial produces only ToolFinished and exact policy reason.
    let tool = Arc::new(FakeTool::new("danger"));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("d1", "danger", "{}")]),
        text_response("done"),
    ]));
    let (mut parts, fixture) = parts(provider, vec![tool.clone()]);
    let authorization = Arc::new(ScriptedAuthorization::denying(&["danger"]));
    parts.authorization = authorization.clone();
    let mut agent = Agent::new(parts).unwrap();
    run(&mut agent, "q", CancellationToken::new()).await;
    let result = tool_result("d1", "danger", ToolStatus::Denied, "denied by test policy");
    assert_eq!(
        fixture.journal.records()[3],
        record(
            3,
            RecordBody::ToolFinished {
                result: result.clone()
            }
        )
    );
    assert!(
        !fixture
            .journal
            .records()
            .iter()
            .any(|r| matches!(r.body, RecordBody::ToolStarted { .. }))
    );
    assert!(
        !fixture
            .events
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStarted { .. }))
    );
    assert!(tool.calls().is_empty());
    assert!(agent.history().contains(&Item::ToolResult(result)));
}

#[tokio::test(start_paused = true)]
async fn permitted_tool_records_start_identity_then_exact_returned_finish() {
    // Spec §4: permitted execution records identity before its exact outcome.
    let tool = Arc::new(
        FakeTool::new("write")
            .with_effect(Effect::WritesFiles)
            .with_identity("writer-impl", "strict")
            .returning(p1_contracts::ToolOutcome::error("bad input")),
    );
    let call = json_call("w1", "write", "not-json");
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![call.clone()]),
        text_response("done"),
    ]));
    let (parts, fixture) = parts(provider, vec![tool.clone()]);
    let mut agent = Agent::new(parts).unwrap();
    run(&mut agent, "q", CancellationToken::new()).await;
    let result = tool_result("w1", "write", ToolStatus::Error, "bad input");
    assert_eq!(
        &fixture.journal.records()[3..5],
        &[
            record(
                3,
                RecordBody::ToolStarted {
                    call_id: "w1".into(),
                    identity: tool.identity().clone()
                }
            ),
            record(
                4,
                RecordBody::ToolFinished {
                    result: result.clone()
                }
            ),
        ]
    );
    let tool_events = fixture
        .events
        .events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool_events,
        vec![
            AgentEvent::ToolStarted { call: call.clone() },
            AgentEvent::ToolFinished { result }
        ]
    );
    assert_eq!(tool.calls(), vec![call]);
    assert_eq!(
        fixture.authorization.seen(),
        vec![("w1".into(), Effect::WritesFiles)]
    );
}

#[tokio::test(start_paused = true)]
async fn mixed_tool_calls_are_processed_sequentially_in_block_order() {
    // Spec §4: available, unavailable, and denied calls remain in block order.
    let good = Arc::new(FakeTool::new("good"));
    let denied = Arc::new(FakeTool::new("denied"));
    let calls = vec![
        json_call("a", "good", "{}"),
        json_call("b", "absent", "{}"),
        json_call("c", "denied", "{}"),
    ];
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(calls),
        text_response("done"),
    ]));
    let (mut parts, fixture) = parts(provider, vec![good.clone(), denied.clone()]);
    let authorization = Arc::new(ScriptedAuthorization::denying(&["denied"]));
    parts.authorization = authorization.clone();
    let mut agent = Agent::new(parts).unwrap();
    run(&mut agent, "q", CancellationToken::new()).await;
    let results = agent
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        vec![
            tool_result("a", "good", ToolStatus::Ok, "good ok"),
            tool_result(
                "b",
                "absent",
                ToolStatus::Unavailable,
                "Tool `absent` is not available."
            ),
            tool_result("c", "denied", ToolStatus::Denied, "denied by test policy"),
        ]
    );
    assert_eq!(
        authorization.seen(),
        vec![
            ("a".into(), Effect::ReadOnly),
            ("c".into(), Effect::ReadOnly)
        ]
    );
    assert_eq!(good.calls().len(), 1);
    assert!(denied.calls().is_empty());
    assert_eq!(fixture.provider.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn tool_name_in_old_history_but_not_assembled_is_not_dispatchable() {
    // Spec §4: history cannot grant tool ownership to this agent.
    let old_call = json_call("old", "ghost", "{}");
    let replacement = vec![Item::Assistant(p1_contracts::AssistantItem {
        origin: origin(),
        blocks: vec![AssistantBlock::ToolCall(old_call)],
    })];
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("new", "ghost", "{}")]),
        text_response("done"),
    ]));
    let (mut parts, fixture) = parts(provider, vec![]);
    parts.context = Arc::new(ReplacingContext::new(0, replacement));
    let mut agent = Agent::new(parts).unwrap();
    run(&mut agent, "q", CancellationToken::new()).await;
    let result = tool_result(
        "new",
        "ghost",
        ToolStatus::Unavailable,
        "Tool `ghost` is not available.",
    );
    assert!(fixture.journal.records().iter().any(|record| record.body
        == RecordBody::ToolFinished {
            result: result.clone()
        }));
    assert!(fixture.authorization.seen().is_empty());
}

async fn cancel_hanging_stream(step: Step) {
    let (mut agent, fixture) = agent_with(vec![step]);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let drained = fixture.provider.drained.clone();
    let task = tokio::spawn(async move {
        let end = agent.run_turn("q".into(), cancel).await;
        (agent, end)
    });
    timeout(LIMIT, drained.notified())
        .await
        .expect("provider did not drain");
    trigger.cancel();
    let (agent, end) = timeout(LIMIT, task)
        .await
        .expect("cancelled turn timed out")
        .expect("turn task panicked");
    assert_eq!(end, TurnEnd::Cancelled);
    assert_eq!(agent.history(), &[user("q")]);
    assert_eq!(
        fixture.journal.records()[2],
        record(
            2,
            RecordBody::AssistantInterrupted {
                reason: InterruptionReason::Cancelled,
                partial_text: "part".into(),
                error: None
            }
        )
    );
    assert_last_finished(&fixture.events.events(), &end);
}

#[tokio::test(start_paused = true)]
async fn cancellation_stops_provider_stream_that_ignores_cancellation() {
    // Spec §4 + §6: core races cancellation even when provider ignores it.
    cancel_hanging_stream(Step::EventsThenHang(vec![StreamEvent::TextDelta {
        block: 0,
        text: "part".into(),
    }]))
    .await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_accepts_provider_cancelled_terminal() {
    // Spec §4 + §6: a cancellation-aware stream ends as Cancelled with partial text.
    cancel_hanging_stream(Step::EventsThenAwaitCancel(vec![StreamEvent::TextDelta {
        block: 0,
        text: "part".into(),
    }]))
    .await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_awaits_running_tool_finishes_remaining_calls_and_agent_is_reusable() {
    // Spec §4 + §6: running tool is awaited, later calls are paired, and next turn works.
    let running = Arc::new(FakeTool::new("running").running_until_cancelled());
    let later = Arc::new(FakeTool::new("later"));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("run", "running", "{}"),
            json_call("later", "later", "{}"),
        ]),
        text_response("reused"),
    ]));
    let (parts, fixture) = parts(provider, vec![running.clone(), later.clone()]);
    let mut agent = Agent::new(parts).unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let started = running.started.clone();
    let task = tokio::spawn(async move {
        let end = agent.run_turn("first".into(), cancel).await;
        (agent, end)
    });
    timeout(LIMIT, started.notified())
        .await
        .expect("tool did not start");
    trigger.cancel();
    let (mut agent, end) = timeout(LIMIT, task)
        .await
        .expect("tool turn timed out")
        .expect("turn task panicked");
    assert_eq!(end, TurnEnd::Cancelled);
    let results = agent
        .history()
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let running_result = tool_result("run", "running", ToolStatus::Cancelled, "cancelled");
    let before_result = tool_result(
        "later",
        "later",
        ToolStatus::Cancelled,
        "Cancelled before execution.",
    );
    assert_eq!(results, vec![running_result.clone(), before_result.clone()]);
    assert!(fixture.journal.records().iter().any(|record| record.body
        == RecordBody::ToolFinished {
            result: before_result.clone()
        }));
    assert!(fixture.events.events().contains(&AgentEvent::ToolFinished {
        result: before_result
    }));
    assert!(later.calls().is_empty());
    assert_eq!(
        agent
            .history()
            .iter()
            .filter(|i| matches!(i, Item::Assistant(_)))
            .count(),
        1
    );
    let calls = agent
        .history()
        .iter()
        .flat_map(|item| match item {
            Item::Assistant(item) => item.tool_calls().map(|call| call.call_id.clone()).collect(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    let paired = agent
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, paired);
    assert_eq!(
        run(&mut agent, "second", CancellationToken::new()).await,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert!(
        agent
            .history()
            .iter()
            .any(|i| matches!(i, Item::User { text } if text == "second"))
    );
    assert_last_finished(
        &fixture.events.events(),
        &TurnEnd::Completed {
            stop: StopReason::EndTurn,
        },
    );
}

#[tokio::test(start_paused = true)]
async fn inbox_delivery_is_ordered_exactly_once_at_request_boundary() {
    // Spec §5: queued inbox messages are committed and delivered once in arrival order.
    let (mut agent, fixture) = agent_with(vec![text_response("answer")]);
    let inbox = agent.inbox();
    assert!(inbox.send(InboxKind::Steering, "first"));
    assert!(inbox.send(InboxKind::Notification, "second"));
    run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        &fixture.journal.records()[2..4],
        &[
            record(
                2,
                RecordBody::Inbox {
                    kind: InboxKind::Steering,
                    text: "first".into()
                }
            ),
            record(
                3,
                RecordBody::Inbox {
                    kind: InboxKind::Notification,
                    text: "second".into()
                }
            ),
        ]
    );
    assert_eq!(
        fixture
            .events
            .events()
            .iter()
            .filter(|e| matches!(e, AgentEvent::InboxDelivered { .. }))
            .collect::<Vec<_>>(),
        vec![&AgentEvent::InboxDelivered { count: 2 }]
    );
    assert_eq!(
        fixture.provider.requests()[0].history,
        vec![
            user("q"),
            Item::Inbox {
                kind: InboxKind::Steering,
                text: "first".into()
            },
            Item::Inbox {
                kind: InboxKind::Notification,
                text: "second".into()
            },
        ]
    );
    assert!(!agent.has_pending_inbox());
}

#[tokio::test(start_paused = true)]
async fn idle_inbox_message_waits_for_next_turn() {
    // Spec §5: sending while idle does not emit or commit until a turn runs.
    let (mut agent, fixture) = agent_with(vec![text_response("answer")]);
    assert!(agent.inbox().send(InboxKind::Notification, "idle"));
    assert!(agent.has_pending_inbox());
    assert!(fixture.journal.records().is_empty());
    assert!(fixture.events.events().is_empty());
    run(&mut agent, "q", CancellationToken::new()).await;
    assert!(
        fixture.provider.requests()[0]
            .history
            .contains(&Item::Inbox {
                kind: InboxKind::Notification,
                text: "idle".into()
            })
    );
}

#[tokio::test(start_paused = true)]
async fn empty_run_inbox_turn_returns_none_without_observation() {
    // Spec §5: empty run_inbox_turn is an exact no-op.
    let (mut agent, fixture) = agent_with(vec![]);
    let result = timeout(LIMIT, agent.run_inbox_turn(CancellationToken::new()))
        .await
        .expect("run_inbox_turn timed out");
    assert_eq!(result, None);
    assert!(fixture.journal.records().is_empty());
    assert!(fixture.events.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn nonempty_run_inbox_turn_has_no_user_input_record() {
    // Spec §5: inbox-only turns run without creating UserInput.
    let (mut agent, fixture) = agent_with(vec![text_response("answer")]);
    assert!(agent.inbox().send(InboxKind::Steering, "continue"));
    let result = timeout(LIMIT, agent.run_inbox_turn(CancellationToken::new()))
        .await
        .expect("run_inbox_turn timed out");
    assert_eq!(
        result,
        Some(TurnEnd::Completed {
            stop: StopReason::EndTurn
        })
    );
    assert!(
        !fixture
            .journal
            .records()
            .iter()
            .any(|r| matches!(r.body, RecordBody::UserInput { .. }))
    );
    assert!(fixture.journal.records().iter().any(|r| r.body
        == RecordBody::Inbox {
            kind: InboxKind::Steering,
            text: "continue".into()
        }));
}

#[tokio::test(start_paused = true)]
async fn inbox_ready_is_immediate_when_pending_and_waits_until_send_otherwise() {
    // Spec §5: readiness neither polls nor consumes and wakes on send.
    let (agent, _) = agent_with(vec![]);
    let inbox = agent.inbox();
    let waiter = tokio::spawn(async move {
        agent.inbox_ready().await;
        agent
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    assert!(inbox.send(InboxKind::Steering, "wake"));
    let agent = timeout(LIMIT, waiter)
        .await
        .expect("inbox_ready did not wake")
        .expect("waiter panicked");
    assert!(agent.has_pending_inbox());
    timeout(LIMIT, agent.inbox_ready())
        .await
        .expect("pending readiness was not immediate");
    assert!(agent.has_pending_inbox());
}

#[tokio::test(start_paused = true)]
async fn inbox_clone_is_sendable_and_send_returns_false_after_agent_drop() {
    // Spec §5: cloned Send handle works cross-task and detects a dropped receiver.
    let (agent, _) = agent_with(vec![]);
    let inbox = agent.inbox();
    let clone = inbox.clone();
    let sent = timeout(
        LIMIT,
        tokio::spawn(async move { clone.send(InboxKind::Notification, "from task") }),
    )
    .await
    .expect("sender task timed out")
    .expect("sender panicked");
    assert!(sent);
    assert!(agent.has_pending_inbox());
    drop(agent);
    assert!(!inbox.send(InboxKind::Steering, "too late"));
}

async fn assert_commit_failure(
    fail_at: u64,
    script: Vec<Step>,
    tool: Option<Arc<FakeTool>>,
) -> (Fixture, Option<Arc<FakeTool>>) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let tools: Vec<Arc<dyn p1_contracts::Tool>> = tool
        .iter()
        .cloned()
        .map(|t| t as Arc<dyn p1_contracts::Tool>)
        .collect();
    let (mut parts, mut fixture) = parts(provider, tools);
    let journal = Arc::new(RecordingJournal::new().failing_at(fail_at));
    parts.journal = journal.clone();
    fixture.journal = journal;
    let mut agent = Agent::new(parts).unwrap();
    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::CommitFailed {
            message: format!("scripted failure at seq {fail_at}")
        }
    );
    assert_last_finished(&fixture.events.events(), &end);
    (fixture, tool)
}

#[tokio::test(start_paused = true)]
async fn environment_commit_failure_stops_before_user_and_provider() {
    // Spec §7: Environment commit failure stops all forward progress.
    let (fixture, _) = assert_commit_failure(0, vec![], None).await;
    assert!(fixture.journal.records().is_empty());
    assert!(fixture.provider.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn user_input_commit_failure_stops_before_provider() {
    // Spec §7: UserInput commit failure stops before request setup.
    let (fixture, _) = assert_commit_failure(1, vec![], None).await;
    assert_eq!(fixture.journal.records().len(), 1);
    assert!(fixture.provider.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn assistant_completed_commit_failure_prevents_authorization_and_execution() {
    // Spec §7: uncommitted AssistantCompleted cannot trigger a tool.
    let tool = Arc::new(FakeTool::new("alpha"));
    let (fixture, tool) = assert_commit_failure(
        2,
        vec![tool_call_response(vec![json_call("a", "alpha", "{}")])],
        Some(tool),
    )
    .await;
    assert!(fixture.authorization.seen().is_empty());
    assert!(tool.unwrap().calls().is_empty());
    assert_eq!(fixture.provider.requests().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn tool_started_commit_failure_prevents_execution() {
    // Spec §7: failed ToolStarted is a hard barrier before side effects.
    let tool = Arc::new(FakeTool::new("alpha"));
    let (fixture, tool) = assert_commit_failure(
        3,
        vec![tool_call_response(vec![json_call("a", "alpha", "{}")])],
        Some(tool),
    )
    .await;
    assert_eq!(
        fixture.authorization.seen(),
        vec![("a".into(), Effect::ReadOnly)]
    );
    assert!(tool.unwrap().calls().is_empty());
    assert_eq!(fixture.provider.requests().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn tool_finished_commit_failure_prevents_next_provider_request() {
    // Spec §7: failed ToolFinished prevents the next model request.
    let tool = Arc::new(FakeTool::new("alpha"));
    let (fixture, tool) = assert_commit_failure(
        4,
        vec![tool_call_response(vec![json_call("a", "alpha", "{}")])],
        Some(tool),
    )
    .await;
    assert_eq!(tool.unwrap().calls().len(), 1);
    assert_eq!(fixture.provider.requests().len(), 1);
}

fn assert_send<T: Send>() {}
fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
fn assert_send_future<F: Future + Send>(_: F) {}

#[tokio::test(start_paused = true)]
async fn public_threading_contracts_hold_at_compile_time() {
    // Spec §8: Agent/future/Inbox expose the required Send/Sync/Clone bounds.
    assert_send::<Agent>();
    assert_send_sync_clone::<Inbox>();
    let (mut agent, _) = agent_with(vec![text_response("answer")]);
    assert_send_future(agent.run_turn("q".into(), CancellationToken::new()));
}
