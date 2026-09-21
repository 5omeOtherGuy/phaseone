//! ADR-0049 stage 2: switching a running session to another model between turns
//! (`Agent::reconfigure`) and resuming a journal on a model whose provider accepts
//! the projected history. Supersedes ADR-0033: a changed origin is no longer its own
//! error — the provider's `validate`, run against the CURRENT or PROJECTED history
//! decides, and it says what it cannot carry.

use std::sync::Arc;

use p1_contracts::{
    AgentEvent, BoxFuture, CancellationToken, Item, ModelOptions, Origin, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RecordBody, RouteDescription, TurnEnd,
};
use p1_core::{Agent, AgentParts, BuildError, Reconfiguration, ResumeError, project};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, json_call, origin, text_response, tool_call_response,
};

/// A scripted provider that describes itself as another origin: a switch or resume
/// target whose route and model differ from the one that recorded the session.
struct Elsewhere {
    inner: ScriptedProvider,
    origin: Origin,
}

impl Provider for Elsewhere {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin.clone(),
            ..self.inner.describe()
        }
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }
    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.inner.stream(request, cancel)
    }
}

fn parts(provider: Arc<dyn Provider>, journal: Arc<RecordingJournal>) -> AgentParts {
    AgentParts {
        provider,
        tools: Vec::new(),
        system_prompt: "prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

fn elsewhere(inner: Arc<ScriptedProvider>) -> Arc<dyn Provider> {
    Arc::new(Elsewhere {
        inner: inner.as_ref().clone(),
        origin: Origin {
            route: "elsewhere-route".into(),
            model: "elsewhere-model".into(),
        },
    })
}

fn environment_records(records: &[p1_contracts::JournalRecord]) -> Vec<&RecordBody> {
    records
        .iter()
        .filter(|record| matches!(record.body, RecordBody::Environment { .. }))
        .map(|record| &record.body)
        .collect()
}

/// One turn on the first provider, leaving `Environment, UserInput, AssistantCompleted`.
async fn first_turn(agent: &mut Agent, journal: &RecordingJournal) {
    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn("hi".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(journal.records().len(), 3);
}

#[tokio::test]
async fn a_switch_commits_the_new_environment_and_asks_the_new_provider_with_the_old_history() {
    let journal = Arc::new(RecordingJournal::new());
    let first = Arc::new(ScriptedProvider::new(vec![text_response("one")]));
    let mut agent =
        Agent::new(parts(first.clone(), journal.clone())).expect("the first environment assembles");
    first_turn(&mut agent, &journal).await;
    let history_before = agent.history().to_vec();

    let second = Arc::new(ScriptedProvider::new(vec![text_response("two")]));
    agent
        .reconfigure(Reconfiguration {
            provider: elsewhere(second.clone()),
            tools: vec![Arc::new(FakeTool::new("read"))],
            system_prompt: "second prompt".into(),
            options: ModelOptions {
                reasoning_effort: Some(p1_contracts::Effort::High),
                ..ModelOptions::default()
            },
            context: Arc::new(PassthroughContext),
        })
        .expect("the new provider accepts the current history");

    // The check is the same one construction runs, but against the CURRENT history.
    let validated = second.validated();
    assert_eq!(validated.len(), 1);
    assert_eq!(
        validated[0].history, history_before,
        "the new provider validates the history the switch happened on"
    );
    assert_eq!(validated[0].system_prompt, "second prompt");
    assert_eq!(
        validated[0].options.reasoning_effort,
        Some(p1_contracts::Effort::High)
    );
    assert_eq!(validated[0].tools.len(), 1);
    assert!(
        first.validated()[0].history.is_empty(),
        "construction still validates against the empty-history first request"
    );

    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn("more".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");

    // The new `Environment` is committed before the turn's input.
    let records = journal.records();
    assert_eq!(records.len(), 6, "{records:#?}");
    match &records[3].body {
        RecordBody::Environment {
            route,
            system_prompt,
            tools,
            options,
        } => {
            assert_eq!(route.origin.route, "elsewhere-route");
            assert_eq!(route.origin.model, "elsewhere-model");
            assert_eq!(system_prompt, "second prompt");
            assert_eq!(tools.len(), 1);
            assert_eq!(options.reasoning_effort, Some(p1_contracts::Effort::High));
        }
        other => panic!("expected the switched Environment at seq 3, saw {other:?}"),
    }
    assert!(matches!(
        records[4].body,
        RecordBody::UserInput { ref text } if text == "more"
    ));

    // The request reached the NEW provider, carrying the OLD history first.
    let requests = second.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        first.requests().len(),
        1,
        "the first provider got turn one only"
    );
    assert_eq!(requests[0].history[0], Item::User { text: "hi".into() });
    assert!(matches!(requests[0].history[1], Item::Assistant(_)));
    assert_eq!(
        requests[0].history[2],
        Item::User {
            text: "more".into()
        }
    );
    assert_eq!(requests[0].system_prompt, "second prompt");
}

#[tokio::test]
async fn a_rejected_reconfigure_leaves_the_old_provider_in_use_and_commits_nothing() {
    let journal = Arc::new(RecordingJournal::new());
    let first = Arc::new(ScriptedProvider::new(vec![
        text_response("one"),
        text_response("still here"),
    ]));
    let mut agent = Agent::new(parts(first.clone(), journal.clone())).unwrap();
    first_turn(&mut agent, &journal).await;

    let refusal = ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        "this route cannot carry a freeform call",
    );
    let second = Arc::new(ScriptedProvider::new(Vec::new()).rejecting_validation(refusal.clone()));
    let error = agent
        .reconfigure(Reconfiguration {
            provider: elsewhere(second.clone()),
            tools: Vec::new(),
            system_prompt: "second prompt".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
        })
        .expect_err("the new provider rejects the history");
    assert_eq!(error, BuildError::ProviderRejected(refusal));
    assert_eq!(
        journal.records().len(),
        3,
        "a refused switch commits nothing"
    );

    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn("more".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");

    assert_eq!(first.requests().len(), 2, "the old provider stays in use");
    assert!(second.requests().is_empty(), "the new one is never asked");
    let records = journal.records();
    assert_eq!(records.len(), 5);
    assert_eq!(
        environment_records(&records).len(),
        1,
        "no new Environment is committed: the environment never changed"
    );
    // The old provider answered the turn after the refused switch: the response of
    // the last turn still carries its origin.
    assert!(matches!(
        records[4].body,
        RecordBody::AssistantCompleted { ref item, .. }
            if item.origin == origin()
    ));
}

#[tokio::test]
async fn duplicate_tool_names_are_rejected_before_the_provider_is_asked() {
    let journal = Arc::new(RecordingJournal::new());
    let first = Arc::new(ScriptedProvider::new(vec![text_response("one")]));
    let mut agent = Agent::new(parts(first, journal.clone())).unwrap();
    first_turn(&mut agent, &journal).await;

    // The target provider would refuse too: the duplicate name is checked FIRST.
    let second = Arc::new(ScriptedProvider::new(Vec::new()).rejecting_validation(
        ProviderError::new(ProviderErrorKind::InvalidRequest, "unreachable"),
    ));
    let error = agent
        .reconfigure(Reconfiguration {
            provider: elsewhere(second.clone()),
            tools: vec![
                Arc::new(FakeTool::new("read")),
                Arc::new(FakeTool::new("read")),
            ],
            system_prompt: "second prompt".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
        })
        .expect_err("two tools with one call name");
    assert_eq!(error, BuildError::DuplicateToolName("read".into()));
    assert!(
        second.validated().is_empty(),
        "the duplicate-name check runs before `provider.validate`"
    );
    assert_eq!(journal.records().len(), 3, "nothing changed");
}

#[tokio::test]
async fn the_first_environment_is_committed_exactly_once_without_a_reconfigure() {
    let journal = Arc::new(RecordingJournal::new());
    let provider = Arc::new(ScriptedProvider::new(vec![
        text_response("one"),
        text_response("two"),
    ]));
    let mut agent = Agent::new(parts(provider.clone(), journal.clone())).unwrap();
    first_turn(&mut agent, &journal).await;
    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn("more".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");

    let records = journal.records();
    assert_eq!(records.len(), 5);
    assert_eq!(environment_records(&records).len(), 1);
    match &records[0].body {
        RecordBody::Environment { route, .. } => {
            assert_eq!(route.origin, origin());
            assert_eq!(records[0].seq, 0);
        }
        other => panic!("expected `Environment` first, saw {other:?}"),
    }
}

/// A switch replaces the model-side parts only: the session's journal, events and
/// authorization policy are the same instances afterwards.
#[tokio::test]
async fn a_switch_keeps_the_journal_the_events_and_the_authorization() {
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let authorization = Arc::new(ScriptedAuthorization::permit_all());
    let first = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("call_1", "read", "{}")]),
        text_response("done"),
    ]));
    let mut agent = Agent::new(AgentParts {
        provider: first,
        tools: vec![Arc::new(FakeTool::new("read"))],
        system_prompt: "prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: authorization.clone(),
        journal: journal.clone(),
        events: events.clone(),
    })
    .unwrap();
    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn("hi".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(authorization.seen().len(), 1);

    let second = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("call_2", "read", "{}")]),
        text_response("done again"),
    ]));
    agent
        .reconfigure(Reconfiguration {
            provider: elsewhere(second.clone()),
            tools: vec![Arc::new(FakeTool::new("read"))],
            system_prompt: "second prompt".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
        })
        .expect("the new provider accepts the current history");
    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn("more".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");

    let calls: Vec<String> = authorization
        .seen()
        .into_iter()
        .map(|(call_id, _)| call_id)
        .collect();
    assert_eq!(
        calls,
        vec!["call_1".to_string(), "call_2".to_string()],
        "the same authorization policy answered both turns"
    );
    assert_eq!(
        events
            .events()
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnStarted))
            .count(),
        2,
        "the same event sink saw both turns"
    );
    let records = journal.records();
    assert_eq!(environment_records(&records).len(), 2);
    assert_eq!(
        second.requests().len(),
        2,
        "the switched turn's two requests went to the new provider"
    );
    // The tool result of the switched turn went through the same journal.
    assert!(records.iter().any(|record| matches!(
        &record.body,
        RecordBody::ToolFinished { result } if result.call_id == "call_2"
    )));
}

/// A session recorded on `fake-route/fake-model`.
async fn recorded_session() -> Vec<p1_contracts::JournalRecord> {
    let journal = Arc::new(RecordingJournal::new());
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello")]));
    let mut agent = Agent::new(parts(provider, journal.clone())).unwrap();
    first_turn(&mut agent, &journal).await;
    journal.records()
}

#[tokio::test]
async fn a_journal_recorded_on_another_origin_resumes_and_commits_its_own_environment() {
    let records = recorded_session().await;
    let projected = project(&records).expect("the journal projects").history;

    let second = Arc::new(ScriptedProvider::new(vec![text_response("two")]));
    let journal = Arc::new(RecordingJournal::new());
    let (mut agent, report) =
        Agent::resume(parts(elsewhere(second.clone()), journal.clone()), &records)
            .expect("the other origin accepts the projected history");

    assert!(report.environment_changed, "another origin is a change");
    assert_eq!(
        second.validated()[0].history,
        projected,
        "resume validates the PROJECTED history, not an empty one"
    );

    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn("more".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");

    let new_records = journal.records();
    assert_eq!(new_records[0].seq, records.len() as u64);
    match &new_records[0].body {
        RecordBody::Environment { route, .. } => {
            assert_eq!(route.origin.route, "elsewhere-route");
            assert_eq!(route.origin.model, "elsewhere-model");
        }
        other => panic!("expected the resumed environment first, saw {other:?}"),
    }
    assert!(matches!(new_records[1].body, RecordBody::UserInput { .. }));
    let requests = second.requests();
    assert_eq!(
        requests[0].history[..projected.len()],
        projected[..],
        "the resumed turn sends the projected history"
    );
    assert_eq!(
        requests[0].history[projected.len()],
        Item::User {
            text: "more".into()
        }
    );
}

#[tokio::test]
async fn a_provider_that_rejects_the_projected_history_fails_resume_and_commits_nothing() {
    let records = recorded_session().await;
    let projected = project(&records).expect("the journal projects").history;

    let refusal = ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        "this route cannot carry the projected history",
    );
    let provider =
        Arc::new(ScriptedProvider::new(Vec::new()).rejecting_validation(refusal.clone()));
    let journal = Arc::new(RecordingJournal::new());

    let error = Agent::resume(parts(provider.clone(), journal.clone()), &records)
        .map(|_| ())
        .expect_err("the provider refuses the history");
    assert_eq!(
        error,
        ResumeError::Build(BuildError::ProviderRejected(refusal))
    );
    assert!(
        journal.records().is_empty(),
        "a refused resume commits nothing"
    );
    assert_eq!(
        provider.validated()[0].history,
        projected,
        "the refusal was about this history"
    );
}
