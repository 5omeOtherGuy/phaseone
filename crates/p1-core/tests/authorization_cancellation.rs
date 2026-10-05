//! Authorization waits belong to each requesting agent's turn, even when
//! agents share a policy whose own cancellation scope is an idle parent.

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision, Item,
    ModelOptions, RecordBody, StopReason, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedProvider, json_call,
    text_response, tool_call_response,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

struct ParkedPolicy(mpsc::UnboundedSender<(String, oneshot::Sender<Decision>)>);

impl AuthorizationPolicy for ParkedPolicy {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            self.0.send((request.call.call_id.clone(), reply)).unwrap();
            answer.await.unwrap()
        })
    }
}

fn agent(policy: Arc<ParkedPolicy>, id: &str) -> (Agent, Arc<FakeTool>, Arc<RecordingJournal>) {
    let tool = Arc::new(FakeTool::new("alpha"));
    let journal = Arc::new(RecordingJournal::new());
    let agent = Agent::new(AgentParts {
        provider: Arc::new(ScriptedProvider::new(vec![
            tool_call_response(vec![
                json_call(id, "alpha", "{}"),
                json_call(&format!("{id}-next"), "alpha", "{}"),
            ]),
            text_response("done"),
        ])),
        tools: vec![tool.clone()],
        system_prompt: "test".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: policy,
        journal: journal.clone(),
        events: Arc::new(RecordingEvents::new()),
    })
    .unwrap();
    (agent, tool, journal)
}

#[tokio::test(start_paused = true)]
async fn cancelling_one_agents_approval_releases_only_its_turn() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let policy = Arc::new(ParkedPolicy(tx));
    let (mut first, first_tool, first_journal) = agent(policy.clone(), "first");
    let (mut second, second_tool, _) = agent(policy, "second");
    let cancel = CancellationToken::new();
    let first_task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let end = first.run_turn("go".into(), cancel).await;
            (first, end)
        }
    });
    let second_task =
        tokio::spawn(async move { second.run_turn("go".into(), CancellationToken::new()).await });
    let mut replies = std::collections::HashMap::new();
    for _ in 0..2 {
        let (id, reply) = timeout(LIMIT, rx.recv()).await.unwrap().unwrap();
        replies.insert(id, reply);
    }
    cancel.cancel();
    let (mut first, end) = timeout(LIMIT, first_task)
        .await
        .expect("cancelled approval kept the agent running")
        .unwrap();
    assert_eq!(end, TurnEnd::Cancelled);
    assert!(
        replies["first"].is_closed(),
        "dead turn still awaits approval"
    );
    assert!(!replies["second"].is_closed());
    assert!(!second_task.is_finished());
    assert!(first_tool.calls().is_empty());
    assert!(
        first_journal
            .records()
            .iter()
            .all(|record| { !matches!(record.body, RecordBody::ToolStarted { .. }) })
    );
    let results: Vec<_> = first
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(result.content, "Cancelled before execution.");
    }
    replies
        .remove("second")
        .unwrap()
        .send(Decision::Permit)
        .unwrap();
    let (id, reply) = timeout(LIMIT, rx.recv()).await.unwrap().unwrap();
    assert_eq!(id, "second-next");
    reply.send(Decision::Permit).unwrap();
    assert_eq!(
        timeout(LIMIT, second_task).await.unwrap().unwrap(),
        TurnEnd::Completed {
            stop: StopReason::EndTurn,
        }
    );
    assert_eq!(second_tool.calls().len(), 2);
    assert_eq!(
        timeout(
            LIMIT,
            first.run_turn("again".into(), CancellationToken::new())
        )
        .await
        .unwrap(),
        TurnEnd::Completed {
            stop: StopReason::EndTurn,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_a_ready_approval() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (mut agent, tool, _) = agent(Arc::new(ParkedPolicy(tx)), "first");
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move { agent.run_turn("go".into(), cancel).await }
    });
    let (_, reply) = timeout(LIMIT, rx.recv()).await.unwrap().unwrap();
    cancel.cancel();
    reply.send(Decision::Permit).unwrap();
    assert_eq!(
        timeout(LIMIT, task).await.unwrap().unwrap(),
        TurnEnd::Cancelled
    );
    assert!(tool.calls().is_empty());
}
