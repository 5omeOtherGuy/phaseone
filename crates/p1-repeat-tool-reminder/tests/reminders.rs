use std::sync::Arc;

use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, Compaction, ContextError,
    ContextInput, ContextPolicy, InboxKind, Item, Origin, Prepared, ToolCall, ToolInput,
    ToolResultItem, ToolStatus,
};
use p1_repeat_tool_reminder::RepeatToolReminder;

struct Passthrough;
impl ContextPolicy for Passthrough {
    fn prepare<'a>(
        &'a self,
        _: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async { Ok(None) })
    }
}

fn call(history: &mut Vec<Item>, name: &str, input: ToolInput) {
    let call_id = format!("call-{}", history.len());
    history.push(Item::Assistant(AssistantItem {
        origin: Origin {
            route: "fake".into(),
            model: "fake".into(),
        },
        blocks: vec![AssistantBlock::ToolCall(ToolCall {
            call_id: call_id.clone(),
            name: name.into(),
            input,
        })],
    }));
    history.push(Item::ToolResult(ToolResultItem {
        call_id,
        name: name.into(),
        status: ToolStatus::Error,
        content: "unchanged result".into(),
    }));
}

async fn prepare(policy: &dyn ContextPolicy, history: &mut Vec<Item>) -> bool {
    let cancel = CancellationToken::new();
    let prepared = policy
        .prepare(ContextInput {
            history,
            last_usage: None,
            cancel: &cancel,
        })
        .await
        .unwrap();
    if let Some(prepared) = prepared {
        *history = prepared.items;
        true
    } else {
        false
    }
}

fn reminders(history: &[Item]) -> Vec<&str> {
    history
        .iter()
        .filter_map(|item| match item {
            Item::Inbox {
                kind: InboxKind::Notification,
                text,
            } if text.starts_with("[p1 repeat-tool reminder]") => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn exact_thresholds_normalized_arguments_results_unchanged_and_resume_idempotent() {
    let policy = RepeatToolReminder::new(Arc::new(Passthrough));
    let mut history = vec![Item::User {
        text: "task".into(),
    }];
    for count in 1..=10 {
        let raw = if count % 2 == 0 {
            r#"{"b":[{"y":2,"x":1},3],"a":0}"#
        } else {
            r#"{ "a":0, "b":[{"x":1,"y":2},3] }"#
        };
        call(&mut history, "read", ToolInput::Json(raw.into()));
        assert_eq!(
            prepare(&policy, &mut history).await,
            [3, 5, 8].contains(&count)
        );
        assert!(
            !prepare(&policy, &mut history).await,
            "a request retry must not add another reminder"
        );
    }
    let notices = reminders(&history);
    assert_eq!(notices.len(), 3);
    for (text, count) in notices.iter().zip([3, 5, 8]) {
        assert!(text.contains(&format!("{count} consecutive times")));
        assert!(text.contains(r#"{"a":0,"b":[{"x":1,"y":2},3]}"#));
    }
    for (index, item) in history.iter().enumerate() {
        if matches!(item, Item::Inbox { .. }) {
            assert!(
                matches!(&history[index - 1], Item::ToolResult(result) if result.content == "unchanged result")
            );
        }
    }
    assert_eq!(
        history
            .iter()
            .filter(|item| matches!(item, Item::ToolResult(_)))
            .count(),
        10
    );
    let resumed = RepeatToolReminder::new(Arc::new(Passthrough));
    assert!(!prepare(&resumed, &mut history).await);
}

#[tokio::test]
async fn different_name_arguments_array_order_and_input_kind_reset_the_chain() {
    let policy = RepeatToolReminder::new(Arc::new(Passthrough));
    let mut history = vec![];
    for (name, input) in [
        ("a", ToolInput::Json("[1,2]".into())),
        ("a", ToolInput::Json("[1,2]".into())),
        ("b", ToolInput::Json("[1,2]".into())),
        ("b", ToolInput::Json("[1,2]".into())),
        ("b", ToolInput::Json("[2,1]".into())),
        ("b", ToolInput::Json("[2,1]".into())),
        ("b", ToolInput::Text("[2,1]".into())),
        ("b", ToolInput::Text("[2,1]".into())),
    ] {
        call(&mut history, name, input);
        assert!(!prepare(&policy, &mut history).await);
    }
    call(&mut history, "b", ToolInput::Text("[2,1]".into()));
    assert!(prepare(&policy, &mut history).await);
    assert_eq!(reminders(&history).len(), 1);
}

#[tokio::test]
async fn new_prompt_and_steering_reset_but_notifications_do_not() {
    let policy = RepeatToolReminder::new(Arc::new(Passthrough));
    let mut history = vec![];
    for boundary in [
        Item::User {
            text: "task".into(),
        },
        Item::Inbox {
            kind: InboxKind::Steering,
            text: "steer".into(),
        },
    ] {
        for _ in 0..2 {
            call(&mut history, "read", ToolInput::Json("broken json".into()));
            assert!(!prepare(&policy, &mut history).await);
        }
        history.push(boundary);
        assert!(!prepare(&policy, &mut history).await);
    }
    for _ in 0..2 {
        call(&mut history, "read", ToolInput::Json("broken json".into()));
        assert!(!prepare(&policy, &mut history).await);
    }
    history.push(Item::Inbox {
        kind: InboxKind::Notification,
        text: "child finished".into(),
    });
    call(&mut history, "read", ToolInput::Json("broken json".into()));
    assert!(prepare(&policy, &mut history).await);
    assert_eq!(reminders(&history).len(), 1);
}

struct Compact;
impl ContextPolicy for Compact {
    fn prepare<'a>(
        &'a self,
        _: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async {
            Ok(Some(Prepared {
                items: vec![Item::User {
                    text: "summary".into(),
                }],
                usage: None,
            }))
        })
    }
    fn compact_now<'a>(
        &'a self,
        _: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        Box::pin(async {
            Ok(Compaction::Replaced {
                prepared: Prepared {
                    items: vec![Item::User {
                        text: "manual summary".into(),
                    }],
                    usage: None,
                },
                tokens_before: 100,
                tokens_after: 1,
            })
        })
    }
}

#[tokio::test]
async fn chain_survives_automatic_and_manual_compaction_with_unicode_preview() {
    let policy = RepeatToolReminder::new(Arc::new(Compact));
    let mut history = vec![];
    let raw = "🦀".repeat(501);
    for _ in 0..2 {
        call(&mut history, "patch", ToolInput::Text(raw.clone()));
        prepare(&policy, &mut history).await;
        assert!(reminders(&history).is_empty());
    }
    let cancel = CancellationToken::new();
    let Compaction::Replaced {
        prepared,
        tokens_before,
        tokens_after,
    } = policy
        .compact_now(ContextInput {
            history: &history,
            last_usage: None,
            cancel: &cancel,
        })
        .await
        .unwrap()
    else {
        panic!("expected compaction")
    };
    assert_eq!((tokens_before, tokens_after), (100, 1));
    history = prepared.items;
    call(&mut history, "patch", ToolInput::Text(raw));
    prepare(&policy, &mut history).await;
    let notices = reminders(&history);
    assert_eq!(notices.len(), 1);
    let preview = notices[0]
        .split("characters): ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap();
    assert_eq!(preview, "🦀".repeat(500));
    assert!(!notices[0].contains(&"🦀".repeat(501)));
}

#[tokio::test]
async fn batch_call_order_not_result_completion_order_determines_repeats() {
    let policy = RepeatToolReminder::new(Arc::new(Passthrough));
    let mut history = vec![];
    for _ in 0..5 {
        call(&mut history, "read", ToolInput::Json("{}".into()));
    }
    let mut blocks = Vec::new();
    let mut results = Vec::new();
    for item in history {
        match item {
            Item::Assistant(assistant) => blocks.extend(assistant.blocks),
            Item::ToolResult(result) => results.push(Item::ToolResult(result)),
            _ => unreachable!(),
        }
    }
    history = vec![Item::Assistant(AssistantItem {
        origin: Origin {
            route: "fake".into(),
            model: "fake".into(),
        },
        blocks,
    })];
    history.extend(results.into_iter().rev());
    prepare(&policy, &mut history).await;
    let notices = reminders(&history);
    assert_eq!(notices.len(), 2);
    assert!(notices[0].contains("5 consecutive times"));
    assert!(notices[1].contains("3 consecutive times"));
}

#[tokio::test]
async fn agent_executes_all_calls_and_sends_only_three_journalled_reminders() {
    use p1_contracts::{ModelOptions, RecordBody, StopReason, TurnEnd};
    use p1_core::{Agent, AgentParts};
    use p1_testkit::{
        FakeTool, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
        json_call, text_response, tool_call_response,
    };

    let mut script: Vec<_> = (1..=10)
        .map(|index| {
            tool_call_response(vec![json_call(
                &format!("call-{index}"),
                "read",
                r#"{"path":"same"}"#,
            )])
        })
        .collect();
    script.push(text_response("done"));
    let provider = Arc::new(ScriptedProvider::new(script));
    let tool = Arc::new(FakeTool::new("read"));
    let journal = Arc::new(RecordingJournal::new());
    let mut agent = Agent::new(AgentParts {
        provider: provider.clone(),
        tools: vec![tool.clone()],
        system_prompt: "test".into(),
        options: ModelOptions::default(),
        context: Arc::new(RepeatToolReminder::new(Arc::new(Passthrough))),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: journal.clone(),
        events: Arc::new(RecordingEvents::new()),
    })
    .unwrap();
    assert_eq!(
        agent
            .run_turn("task".into(), CancellationToken::new())
            .await,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(tool.calls().len(), 10);
    let requests = provider.requests();
    assert_eq!(requests.len(), 11);
    for (index, request) in requests.iter().enumerate() {
        let expected = [3, 5, 8]
            .into_iter()
            .filter(|threshold| index >= *threshold)
            .count();
        assert_eq!(reminders(&request.history).len(), expected);
    }
    assert_eq!(
        journal
            .records()
            .iter()
            .filter(|record| matches!(&record.body, RecordBody::ContextReplaced { .. }))
            .count(),
        3
    );
}
