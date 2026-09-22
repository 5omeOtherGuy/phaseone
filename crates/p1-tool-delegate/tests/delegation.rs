//! Must-pass behaviour tests for the delegation slice.
//!
//! Every child is a real `p1_core::Agent` over a `ScriptedProvider` (or a thin
//! gated/drop-counting wrapper around one) built by a test factory. All waits run
//! under `tokio::time::timeout`, the runtime is paused, and there are no sleeps:
//! tasks are driven by explicit `yield_now` boundaries and `Notify`s.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::{
    BoxFuture, CancellationToken, Item, ModelOptions, Provider, ProviderError, ProviderRequest,
    ProviderStream, RouteDescription, StopReason, Tool, ToolCall, ToolContext, ToolInput,
    ToolOutcome, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, Step, json_call, text_response, tool_call_response,
};
use p1_tool_delegate::all;
use p1_workers::{
    AgentFactory, ChildAgent, ChildId, ChildSpec, ChildStatus, FinishReport, InProcessWorkers,
    WorkerError, WorkerReport, WorkerService,
};
use tokio::sync::Notify;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(30);
const NOTICE_W1_COMPLETED: &str =
    "Worker w1 finished (completed). Use worker_result to read its result.";

async fn within<F: std::future::Future>(future: F) -> F::Output {
    timeout(LIMIT, future).await.expect("operation timed out")
}

fn spec() -> ChildSpec {
    ChildSpec {
        environment: "child".into(),
        task: "do it".into(),
        tools: vec!["read".into()],
        workspace: None,
    }
}

/// The tool modules a test parent may grant — the host supplies this list; the tool
/// crate itself names no concrete tool. `finish` and `worker_*` are deliberately
/// absent, exactly as the host builds it.
fn grantable() -> Vec<String> {
    vec!["edit".into(), "read".into(), "shell".into()]
}

/// The environments a test worker may run.
fn environments() -> Vec<String> {
    vec!["child".into()]
}

// ---------------------------------------------------------------- agents

fn build_agent(provider: Arc<dyn Provider>, prompt: &str, tools: Vec<Arc<dyn Tool>>) -> Agent {
    let parts = AgentParts {
        provider,
        tools,
        system_prompt: prompt.to_string(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(RecordingJournal::new()),
        events: Arc::new(RecordingEvents::new()),
    };
    Agent::new(parts).expect("agent build")
}

fn child_agent(
    provider: Arc<dyn Provider>,
    prompt: &str,
    tools: Vec<Arc<dyn Tool>>,
    description: &str,
) -> ChildAgent {
    ChildAgent {
        agent: build_agent(provider, prompt, tools),
        description: description.to_string(),
        // These test agents have no host tap; an empty report is what a factory
        // without one returns (ADR-0050 item 6).
        report: Arc::new(WorkerReport::default),
        // No host assembly behind them either, so nothing can re-assemble them with
        // a larger grant (ADR-0050 item 6): `add_tools` is refused.
        regrant: None,
    }
}

/// A factory whose children consume `scripts` in order. Records each child's
/// provider so a test can inspect the requests it was given.
fn scripted_factory(
    prompt: &str,
    tools: Vec<Arc<dyn Tool>>,
    scripts: Vec<Vec<Step>>,
    description: &str,
) -> (AgentFactory, Arc<Mutex<Vec<ScriptedProvider>>>) {
    let queue = Arc::new(Mutex::new(VecDeque::from(scripts)));
    let providers = Arc::new(Mutex::new(Vec::new()));
    let providers_for_factory = Arc::clone(&providers);
    let prompt = prompt.to_string();
    let tools = Arc::new(tools);
    let description = description.to_string();
    let factory: AgentFactory = Arc::new(move |_spec: &ChildSpec| {
        let script = queue.lock().unwrap().pop_front().unwrap_or_default();
        let provider = ScriptedProvider::new(script);
        providers_for_factory.lock().unwrap().push(provider.clone());
        Ok(child_agent(
            Arc::new(provider),
            &prompt,
            tools.as_ref().clone(),
            &description,
        ))
    });
    (factory, providers)
}

// ---------------------------------------------------------------- tools

fn tool_by_name(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.declaration().name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no tool named {name}"))
}

async fn exec_tool(tool: &Arc<dyn Tool>, name: &str, input: &str) -> ToolOutcome {
    let call = ToolCall {
        call_id: "call".into(),
        name: name.into(),
        input: ToolInput::Json(input.into()),
    };
    within(tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    ))
    .await
}

fn tool_result(agent: &Agent, name: &str) -> Option<p1_contracts::ToolResultItem> {
    agent.history().iter().find_map(|item| match item {
        Item::ToolResult(result) if result.name == name => Some(result.clone()),
        _ => None,
    })
}

fn notification_texts(agent: &Agent) -> Vec<String> {
    agent
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::Inbox {
                kind: p1_contracts::InboxKind::Notification,
                text,
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------- gated provider (test c)

/// A `ScriptedProvider` whose FIRST `stream` call waits for a `Notify`. Lets a
/// test hold a child mid-turn while the parent blocks in `worker_result{wait}`.
struct GatedProvider {
    inner: ScriptedProvider,
    gate: Arc<Notify>,
    gated: AtomicBool,
}

impl GatedProvider {
    fn new(inner: ScriptedProvider) -> Self {
        Self {
            inner,
            gate: Arc::new(Notify::new()),
            gated: AtomicBool::new(false),
        }
    }
}

impl Provider for GatedProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            if !self.gated.swap(true, Ordering::SeqCst) {
                self.gate.notified().await;
            }
            self.inner.stream(request, cancel).await
        })
    }
}

// ---------------------------------------------------------------- drop counter (test i)

/// A `ScriptedProvider` that bumps a counter when the child's `Agent` drops it.
/// After `shutdown().await` the counter proves no child task is left alive.
struct DropCountingProvider {
    inner: ScriptedProvider,
    dropped: Arc<AtomicUsize>,
}

impl Drop for DropCountingProvider {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl Provider for DropCountingProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
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

// ================================================================ a

#[tokio::test(start_paused = true)]
async fn child_finishing_mid_turn_reaches_parent_next_request() {
    let (factory, _child_providers) = scripted_factory(
        "child prompt",
        vec![],
        vec![vec![text_response("child answer")]],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let parent_provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"child","task":"do it","tools":["read"]}"#,
        )]),
        text_response("parent done"),
    ]);
    let mut parent = build_agent(
        Arc::new(parent_provider.clone()),
        "parent prompt",
        tools.clone(),
    );
    workers.set_parent_inbox(parent.inbox());

    let end = within(parent.run_turn("go".into(), CancellationToken::new())).await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    // The child finished during the `worker_start` tool call. The notification is
    // delivered at request 2's inbox boundary, and the result is already
    // retrievable there: it was stored BEFORE the notification was sent (7a).
    let requests = parent_provider.requests();
    assert_eq!(requests.len(), 2, "the parent makes two requests");
    assert!(
        requests[1].history.iter().any(|item| matches!(
            item,
            Item::Inbox { kind: p1_contracts::InboxKind::Notification, text }
                if text == NOTICE_W1_COMPLETED
        )),
        "request 2 history: {:?}",
        requests[1].history
    );

    let result = exec_tool(
        &tool_by_name(&tools, "worker_result"),
        "worker_result",
        r#"{"id":"w1"}"#,
    )
    .await;
    assert_eq!(result.status, ToolStatus::Ok);
    // An empty report (no tap): the three report lines are still there, the
    // missing-call line is omitted, and the status and text are unchanged.
    assert_eq!(
        result.content,
        "tools: \nfinish: not called\n---\nWorker w1: finished\n\nchild answer"
    );
}

// ================================================================ b

#[tokio::test(start_paused = true)]
async fn child_finishing_while_parent_idle_wakes_it() {
    let (factory, _) = scripted_factory(
        "child prompt",
        vec![],
        vec![vec![text_response("child answer")]],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let parent_provider = ScriptedProvider::new(vec![text_response("after inbox")]);
    let mut parent = build_agent(Arc::new(parent_provider.clone()), "parent prompt", tools);
    workers.set_parent_inbox(parent.inbox());

    // The child completes during `start`; the parent has not run any turn yet.
    let id = within(workers.start(spec())).await.unwrap();
    assert_eq!(id, ChildId("w1".into()));
    assert!(parent.has_pending_inbox(), "the notification is pending");

    within(parent.inbox_ready()).await;
    let end = within(parent.run_inbox_turn(CancellationToken::new())).await;
    assert_eq!(
        end,
        Some(TurnEnd::Completed {
            stop: StopReason::EndTurn
        })
    );
    assert!(
        !parent.has_pending_inbox(),
        "delivered exactly once, not left pending"
    );
    assert_eq!(notification_texts(&parent), vec![NOTICE_W1_COMPLETED]);
}

// ================================================================ c

#[tokio::test(start_paused = true)]
async fn child_finishing_while_parent_waits_in_worker_result() {
    let gated = Arc::new(GatedProvider::new(ScriptedProvider::new(vec![
        text_response("child answer"),
    ])));
    let gated_for_factory = Arc::clone(&gated);
    let factory: AgentFactory = Arc::new(move |_spec: &ChildSpec| {
        let provider: Arc<dyn Provider> = gated_for_factory.clone();
        Ok(child_agent(provider, "child prompt", vec![], "route/model"))
    });
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let parent_provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        text_response("parent done"),
    ]);
    let mut parent = build_agent(Arc::new(parent_provider.clone()), "parent prompt", tools);
    workers.set_parent_inbox(parent.inbox());

    // The child starts and blocks in its provider, so it is still RUNNING.
    let id = within(workers.start(spec())).await.unwrap();
    assert!(matches!(
        within(workers.status(&id)).await.unwrap(),
        ChildStatus::Running
    ));

    let parent_task = tokio::spawn(async move {
        let end = parent.run_turn("go".into(), CancellationToken::new()).await;
        (end, parent)
    });
    // Drive the parent until it is inside `worker_result{wait:true}`.
    let mut spins = 0;
    while parent_provider.requests().is_empty() && spins < 100 {
        tokio::task::yield_now().await;
        spins += 1;
    }
    assert_eq!(parent_provider.requests().len(), 1);
    tokio::task::yield_now().await;

    // Release the child; it finishes while the parent is blocked in the wait.
    gated.gate.notify_one();
    let (end, parent) = within(parent_task).await.expect("parent task");
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    // The tool returned the retained result ...
    let result = tool_result(&parent, "worker_result").expect("worker_result in history");
    assert_eq!(result.status, ToolStatus::Ok);
    assert_eq!(
        result.content,
        "tools: \nfinish: not called\n---\nWorker w1: finished\n\nchild answer"
    );
    // ... and the notification still arrived exactly once afterwards.
    assert_eq!(notification_texts(&parent), vec![NOTICE_W1_COMPLETED]);
}

// ================================================================ d

#[tokio::test(start_paused = true)]
async fn missed_notification_still_leaves_the_result_retrievable() {
    let (factory, _) = scripted_factory(
        "child prompt",
        vec![],
        vec![vec![text_response("late answer")]],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    // No parent inbox is ever set: the notification has nowhere to go.
    let id = within(workers.start(spec())).await.unwrap();
    tokio::task::yield_now().await;

    let result = exec_tool(
        &tool_by_name(&tools, "worker_result"),
        "worker_result",
        r#"{"id":"w1"}"#,
    )
    .await;
    assert_eq!(result.status, ToolStatus::Ok);
    assert_eq!(
        result.content,
        "tools: \nfinish: not called\n---\nWorker w1: finished\n\nlate answer"
    );
    assert!(matches!(
        within(workers.status(&id)).await.unwrap(),
        ChildStatus::Finished(_)
    ));
}

// ================================================================ e

#[tokio::test(start_paused = true)]
async fn child_sees_only_its_own_prompt_and_tools() {
    let (factory, child_providers) = scripted_factory(
        "child system prompt",
        vec![Arc::new(FakeTool::new("child_tool"))],
        vec![vec![text_response("child answer")]],
        "child-route/child-model",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let mut tools = all(service, grantable(), environments());
    tools.push(Arc::new(FakeTool::new("parent_tool")));

    let parent_provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"child","task":"the only task","tools":["read"]}"#,
        )]),
        text_response("parent done"),
    ]);
    let mut parent = build_agent(Arc::new(parent_provider), "parent system prompt", tools);
    workers.set_parent_inbox(parent.inbox());
    within(parent.run_turn("go".into(), CancellationToken::new())).await;

    let requests = child_providers.lock().unwrap()[0].requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.system_prompt, "child system prompt");
    let names: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, ["child_tool"]);
    // The task text is the child's ONLY user item; none of the parent's history
    // (assistant items, tool results, inbox notifications) is present.
    assert_eq!(
        request.history,
        vec![Item::User {
            text: "the only task".into()
        }]
    );
}

// ================================================================ f

#[tokio::test(start_paused = true)]
async fn worker_continue_keeps_the_child_history() {
    let (factory, child_providers) = scripted_factory(
        "child prompt",
        vec![],
        vec![vec![
            text_response("first answer"),
            text_response("second answer"),
        ]],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let parent_provider = ScriptedProvider::new(vec![text_response("ack")]);
    let mut parent = build_agent(Arc::new(parent_provider), "parent prompt", tools);
    workers.set_parent_inbox(parent.inbox());

    let id = within(workers.start(spec())).await.unwrap();
    assert!(matches!(
        within(workers.status(&id)).await.unwrap(),
        ChildStatus::Finished(_)
    ));

    // Repair in the SAME session.
    within(workers.continue_child(&id, "please fix it".into(), Vec::new()))
        .await
        .unwrap();
    let status = within(workers.wait(&id, CancellationToken::new()))
        .await
        .unwrap();
    assert!(matches!(status, ChildStatus::Finished(_)));

    let requests = child_providers.lock().unwrap()[0].requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].history[0],
        Item::User {
            text: "do it".into()
        }
    );
    match &requests[1].history[1] {
        Item::Assistant(item) => assert_eq!(item.text(), "first answer"),
        other => panic!("expected the first answer, got {other:?}"),
    }
    assert_eq!(
        requests[1].history[2],
        Item::User {
            text: "please fix it".into()
        }
    );

    // One notification per completed turn, and no more (invariant 7b).
    within(parent.run_inbox_turn(CancellationToken::new())).await;
    assert_eq!(
        notification_texts(&parent),
        vec![NOTICE_W1_COMPLETED, NOTICE_W1_COMPLETED]
    );
}

// ================================================================ g

#[tokio::test(start_paused = true)]
async fn limit_reached_then_a_start_succeeds_after_completion() {
    let (factory, _) = scripted_factory(
        "child prompt",
        vec![],
        vec![
            vec![Step::EventsThenAwaitCancel(vec![])],
            vec![text_response("second answer")],
        ],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 1);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let first = within(workers.start(spec())).await.unwrap();
    assert!(matches!(
        within(workers.status(&first)).await.unwrap(),
        ChildStatus::Running
    ));
    // The bound is reached: an error, never a queue.
    assert_eq!(
        within(workers.start(spec())).await,
        Err(WorkerError::LimitReached { max: 1 })
    );
    let outcome = exec_tool(
        &tool_by_name(&tools, "worker_start"),
        "worker_start",
        r#"{"environment":"child","task":"do it","tools":["read"]}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Error);
    assert_eq!(
        outcome.content,
        "Cannot start another worker: 1 are already running."
    );

    // After the first child stops running, a new start succeeds.
    within(workers.cancel(&first)).await.unwrap();
    assert_eq!(
        within(workers.wait(&first, CancellationToken::new()))
            .await
            .unwrap(),
        ChildStatus::Cancelled
    );
    let third = within(workers.start(spec())).await.unwrap();
    assert_eq!(third, ChildId("w2".into()));
}

// ================================================================ h

#[tokio::test(start_paused = true)]
async fn cancel_of_a_hanging_child_reports_cancelled() {
    let (factory, _) = scripted_factory(
        "child prompt",
        vec![],
        vec![vec![Step::EventsThenAwaitCancel(vec![])]],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let parent_provider = ScriptedProvider::new(vec![text_response("ack")]);
    let mut parent = build_agent(Arc::new(parent_provider), "parent prompt", tools.clone());
    workers.set_parent_inbox(parent.inbox());

    let id = within(workers.start(spec())).await.unwrap();
    assert!(matches!(
        within(workers.status(&id)).await.unwrap(),
        ChildStatus::Running
    ));
    within(workers.cancel(&id)).await.unwrap();
    assert_eq!(
        within(workers.wait(&id, CancellationToken::new()))
            .await
            .unwrap(),
        ChildStatus::Cancelled
    );
    let outcome = exec_tool(
        &tool_by_name(&tools, "worker_cancel"),
        "worker_cancel",
        r#"{"id":"w1"}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Ok);
    assert_eq!(outcome.content, "Worker w1 cancelled.");

    within(parent.run_inbox_turn(CancellationToken::new())).await;
    assert_eq!(
        notification_texts(&parent),
        vec!["Worker w1 finished (cancelled). Use worker_result to read its result."]
    );
}

// ================================================================ i

#[tokio::test(start_paused = true)]
async fn shutdown_ends_every_child_and_later_calls_fail() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let scripts: Arc<Mutex<VecDeque<Vec<Step>>>> = Arc::new(Mutex::new(VecDeque::from(vec![
        vec![Step::EventsThenAwaitCancel(vec![])],
        vec![Step::EventsThenAwaitCancel(vec![])],
    ])));
    let dropped_for_factory = Arc::clone(&dropped);
    let factory: AgentFactory = Arc::new(move |_spec: &ChildSpec| {
        let script = scripts.lock().unwrap().pop_front().unwrap_or_default();
        let provider: Arc<dyn Provider> = Arc::new(DropCountingProvider {
            inner: ScriptedProvider::new(script),
            dropped: Arc::clone(&dropped_for_factory),
        });
        Ok(child_agent(provider, "child prompt", vec![], "route/model"))
    });
    let workers = InProcessWorkers::new(factory, 2);

    let w1 = within(workers.start(spec())).await.unwrap();
    let w2 = within(workers.start(spec())).await.unwrap();
    assert_eq!(
        (w1.clone(), w2.clone()),
        (ChildId("w1".into()), ChildId("w2".into()))
    );
    assert!(matches!(
        within(workers.status(&w1)).await.unwrap(),
        ChildStatus::Running
    ));
    assert!(matches!(
        within(workers.status(&w2)).await.unwrap(),
        ChildStatus::Running
    ));

    within(workers.shutdown()).await;
    // Both child tasks ended, so both child `Agent`s (and their providers) dropped.
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        2,
        "a child task outlived shutdown"
    );

    // Every fallible call now reports ShutDown.
    assert_eq!(
        within(workers.status(&w1)).await,
        Err(WorkerError::ShutDown)
    );
    assert_eq!(
        within(workers.cancel(&w1)).await,
        Err(WorkerError::ShutDown)
    );
    assert_eq!(
        within(workers.continue_child(&w1, "x".into(), Vec::new())).await,
        Err(WorkerError::ShutDown)
    );
    assert_eq!(
        within(workers.wait(&w1, CancellationToken::new())).await,
        Err(WorkerError::ShutDown)
    );
    assert_eq!(
        within(workers.start(spec())).await,
        Err(WorkerError::ShutDown)
    );
    // `list` is infallible by the spec signature; nothing is left RUNNING.
    assert!(
        within(workers.list())
            .await
            .iter()
            .all(|(_, status)| !matches!(status, ChildStatus::Running))
    );
}

// ================================================================ j

#[tokio::test(start_paused = true)]
async fn without_delegation_worker_start_is_unavailable() {
    // The harness works with the delegation module absent: a plain agent that
    // invents `worker_start` gets `Unavailable`, never a dispatch.
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"x","task":"y"}"#,
        )]),
        text_response("done"),
    ]);
    let mut agent = build_agent(
        Arc::new(provider),
        "plain prompt",
        vec![Arc::new(FakeTool::new("read"))],
    );

    let end = within(agent.run_turn("go".into(), CancellationToken::new())).await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    let result = tool_result(&agent, "worker_start").expect("tool result in history");
    assert_eq!(result.status, ToolStatus::Unavailable);
    assert_eq!(result.content, "Tool `worker_start` is not available.");
}

// ================================================================ k

#[tokio::test(start_paused = true)]
async fn invalid_input_is_reported_for_every_tool() {
    let (factory, _) = scripted_factory("child", vec![], vec![], "route/model");
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let garbage: [&str; 6] = [
        "",
        "null",
        "[]",
        "{\"id\": 5}",
        "{\"id\":\"w1\",\"unknown\":1}",
        "not json at all",
    ];
    let cases: Vec<(&str, Vec<&str>)> = vec![
        (
            "worker_start",
            vec![
                "",
                "null",
                "[]",
                "{\"environment\": 5}",
                "{\"environment\":\"x\",\"task\":\"y\",\"extra\":1}",
                "{\"environment\":\"x\"}",
            ],
        ),
        ("worker_result", garbage.to_vec()),
        (
            "worker_continue",
            vec![
                "{\"id\":\"w1\"}",
                "{\"id\":\"w1\",\"message\":\"m\",\"extra\":1}",
                "{\"message\":\"m\"}",
            ],
        ),
        ("worker_cancel", garbage.to_vec()),
    ];

    for (name, inputs) in cases {
        let tool = tool_by_name(&tools, name);
        for input in inputs {
            let outcome = exec_tool(&tool, name, input).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{name} {input:?}");
            assert!(
                outcome
                    .content
                    .starts_with(&format!("Invalid input for {name}: ")),
                "{name} {input:?} -> {outcome:?}"
            );
        }
        // A freeform `ToolInput::Text` is invalid input too, never a panic.
        let call = ToolCall {
            call_id: "call".into(),
            name: name.into(),
            input: ToolInput::Text("freeform".into()),
        };
        let outcome = within(tool.execute(
            &call,
            ToolContext {
                cancel: CancellationToken::new(),
            },
        ))
        .await;
        assert_eq!(outcome.status, ToolStatus::Error, "{name} text input");
        assert!(
            outcome
                .content
                .starts_with(&format!("Invalid input for {name}: ")),
            "{name} text input -> {outcome:?}"
        );
    }
}

// ================================================================ extra invariants

#[tokio::test(start_paused = true)]
async fn wait_is_immediate_for_a_finished_child_and_cancel_returns_running() {
    let (factory, _) = scripted_factory(
        "child",
        vec![],
        vec![vec![text_response("done")]],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 2);

    // A finished child resolves immediately, even with a cancel that already fired.
    let finished = within(workers.start(spec())).await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        within(workers.wait(&finished, cancel)).await.unwrap(),
        ChildStatus::Finished(_)
    ));

    // A running child's wait resolves `Running` when its cancel fires first.
    let (factory, _) = scripted_factory(
        "child",
        vec![],
        vec![vec![Step::EventsThenAwaitCancel(vec![])]],
        "route/model",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let running = within(workers.start(spec())).await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        within(workers.wait(&running, cancel)).await.unwrap(),
        ChildStatus::Running
    );

    // Unknown ids are reported, not panics.
    assert_eq!(
        within(workers.status(&ChildId("nope".into()))).await,
        Err(WorkerError::UnknownChild)
    );
    assert_eq!(
        within(workers.cancel(&ChildId("nope".into()))).await,
        Err(WorkerError::UnknownChild)
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_environment_and_describe_are_reported() {
    let factory: AgentFactory = Arc::new(|_spec: &ChildSpec| Err("no such route".to_string()));
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    assert_eq!(
        within(workers.start(spec())).await,
        Err(WorkerError::InvalidEnvironment("no such route".into()))
    );
    let outcome = exec_tool(
        &tool_by_name(&tools, "worker_start"),
        "worker_start",
        r#"{"environment":"child","task":"do it","tools":["read"]}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Error);
    assert_eq!(outcome.content, "Cannot start worker: no such route");

    // A good factory reports the route/model description for `worker_start`.
    let (factory, _) = scripted_factory(
        "child",
        vec![],
        vec![vec![Step::EventsThenAwaitCancel(vec![])]],
        "openai-codex-responses/gpt-5.6-sol",
    );
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());
    let outcome = exec_tool(
        &tool_by_name(&tools, "worker_start"),
        "worker_start",
        r#"{"environment":"child","task":"do it","tools":["read"]}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Ok);
    assert_eq!(
        outcome.content,
        "Started worker w1 on openai-codex-responses/gpt-5.6-sol with tools: read, finish. \
         You will be notified when it finishes."
    );
}

#[tokio::test(start_paused = true)]
async fn review_continuation_obeys_global_limit() {
    let (factory, _) = scripted_factory(
        "child",
        vec![],
        vec![
            vec![text_response("first"), Step::EventsThenAwaitCancel(vec![])],
            vec![Step::EventsThenAwaitCancel(vec![])],
        ],
        "r/m",
    );
    let workers = InProcessWorkers::new(factory, 1);
    let first = workers.start(spec()).await.unwrap();
    workers
        .wait(&first, CancellationToken::new())
        .await
        .unwrap();
    let _second = workers.start(spec()).await.unwrap();
    let result = workers
        .continue_child(&first, "repair".into(), Vec::new())
        .await;
    tokio::task::yield_now().await;
    let running = workers
        .list()
        .await
        .into_iter()
        .filter(|(_, s)| matches!(s, ChildStatus::Running))
        .count();
    workers.shutdown().await;
    assert_eq!(
        result,
        Err(WorkerError::LimitReached { max: 1 }),
        "running count {running}"
    );
}

#[tokio::test(start_paused = true)]
async fn review_cancel_immediately_after_continue_is_retained() {
    let (factory, _) = scripted_factory(
        "child",
        vec![],
        vec![vec![
            text_response("first"),
            Step::EventsThenAwaitCancel(vec![]),
        ]],
        "r/m",
    );
    let workers = InProcessWorkers::new(factory, 1);
    let id = workers.start(spec()).await.unwrap();
    workers.wait(&id, CancellationToken::new()).await.unwrap();
    workers
        .continue_child(&id, "repair".into(), Vec::new())
        .await
        .unwrap();
    workers.cancel(&id).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        workers.wait(&id, CancellationToken::new()),
    )
    .await;
    workers.shutdown().await;
    assert!(
        matches!(result, Ok(Ok(ChildStatus::Cancelled))),
        "immediate cancellation was lost: {result:?}"
    );
}

// ================================================================ the grant (ADR-0050 item 3)

/// A factory that records every `ChildSpec` it is asked to build, so a test can see
/// the grant the tool passed on. Each child answers with one text response.
fn recording_factory() -> (AgentFactory, Arc<Mutex<Vec<ChildSpec>>>) {
    let specs = Arc::new(Mutex::new(Vec::new()));
    let specs_for_factory = Arc::clone(&specs);
    let factory: AgentFactory = Arc::new(move |spec: &ChildSpec| {
        specs_for_factory.lock().unwrap().push(spec.clone());
        Ok(child_agent(
            Arc::new(ScriptedProvider::new(vec![text_response("child done")])),
            "child",
            vec![],
            "route/model",
        ))
    });
    (factory, specs)
}

fn start_schema_of(tool: &Arc<dyn Tool>) -> serde_json::Value {
    match &tool.declaration().kind {
        p1_contracts::DeclarationKind::Function { input_schema } => input_schema.clone(),
        other => panic!("worker_start must be a function tool, got {other:?}"),
    }
}

/// The `tools` schema is the grant: required, at least one, unique, and every item
/// one of the host's module names; `environment` is the host's environment list.
#[tokio::test(start_paused = true)]
async fn worker_start_schema_is_the_grant_and_the_environments() {
    let factory: AgentFactory = Arc::new(|_spec: &ChildSpec| Err("unused".to_string()));
    let workers = InProcessWorkers::new(factory, 1);
    let service: Arc<dyn WorkerService> = workers;
    let tools = all(service, grantable(), environments());
    let schema = start_schema_of(&tool_by_name(&tools, "worker_start"));

    assert_eq!(
        schema["required"],
        serde_json::json!(["environment", "task", "tools"]),
        "all three fields are required"
    );
    assert_eq!(
        schema["properties"]["tools"]["minItems"],
        serde_json::json!(1)
    );
    assert_eq!(
        schema["properties"]["tools"]["uniqueItems"],
        serde_json::json!(true)
    );
    assert_eq!(
        schema["properties"]["tools"]["items"]["enum"],
        serde_json::json!(["edit", "read", "shell"])
    );
    assert_eq!(
        schema["properties"]["environment"]["enum"],
        serde_json::json!(["child"])
    );
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
}

/// An empty, missing or unknown grant is refused with the exact text the model can
/// act on, and NOTHING is started (the service keeps no child).
#[tokio::test(start_paused = true)]
async fn worker_start_refuses_a_missing_empty_or_unknown_grant_without_starting() {
    let (factory, specs) = recording_factory();
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());
    let start = tool_by_name(&tools, "worker_start");
    let expected = "`tools` is required: list every tool module the worker needs, from: \
                    edit, read, shell";

    for input in [
        r#"{"environment":"child","task":"do it"}"#,
        r#"{"environment":"child","task":"do it","tools":[]}"#,
    ] {
        let outcome = exec_tool(&start, "worker_start", input).await;
        assert_eq!(outcome.status, ToolStatus::Error, "{input}");
        assert_eq!(outcome.content, expected, "{input}");
    }

    let outcome = exec_tool(
        &start,
        "worker_start",
        r#"{"environment":"child","task":"do it","tools":["bogus"]}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Error);
    assert!(
        outcome.content.contains("`bogus`") && outcome.content.contains("edit, read, shell"),
        "an unknown module names it and the valid list: {}",
        outcome.content
    );

    assert!(
        specs.lock().unwrap().is_empty(),
        "a refused start must not reach the factory"
    );
    assert!(within(workers.list()).await.is_empty());
}

/// Duplicates are removed keeping the first occurrence's order, and the grant is
/// passed into the child spec exactly once per module.
#[tokio::test(start_paused = true)]
async fn worker_start_removes_duplicate_tools_keeping_first_order() {
    let (factory, specs) = recording_factory();
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers;
    let tools = all(service, grantable(), environments());
    let outcome = exec_tool(
        &tool_by_name(&tools, "worker_start"),
        "worker_start",
        r#"{"environment":"child","task":"do it","tools":["shell","read","shell","edit","read"]}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Ok);
    assert_eq!(
        outcome.content,
        "Started worker w1 on route/model with tools: shell, read, edit, finish. You will be \
         notified when it finishes."
    );
    assert_eq!(
        specs.lock().unwrap()[0].tools,
        vec!["shell".to_string(), "read".to_string(), "edit".to_string()]
    );
}

/// The grant reaches the factory as the child's spec, and the success text names the
/// grant plus `finish` (the prefix `workers_started_in` reads is unchanged).
#[tokio::test(start_paused = true)]
async fn worker_start_passes_the_grant_into_the_child_spec() {
    let (factory, specs) = recording_factory();
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let parent_provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"child","task":"do it","tools":["read","shell"]}"#,
        )]),
        text_response("parent done"),
    ]);
    let mut parent = build_agent(Arc::new(parent_provider.clone()), "parent", tools);
    workers.set_parent_inbox(parent.inbox());
    within(parent.run_turn("go".into(), CancellationToken::new())).await;

    let recorded = specs.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].tools,
        vec!["read".to_string(), "shell".to_string()]
    );
    // The same grant the child is assembled with is named back to the parent.
    assert!(
        parent_provider.requests()[1]
            .history
            .iter()
            .any(|item| matches!(
                item,
                Item::ToolResult(result)
                    if result.content
                        == "Started worker w1 on route/model with tools: read, shell, finish. You \
                            will be notified when it finishes."
            )),
        "request 2 history: {:?}",
        parent_provider.requests()[1].history
    );
}

// ================================================================ adding tools (ADR-0050 item 6)

/// `worker_continue`'s `add_tools` is the same grantable list `worker_start` uses:
/// optional, unique items, every one a host module name. An unknown module is
/// refused with the valid list, and nothing reaches the worker service.
#[tokio::test(start_paused = true)]
async fn worker_continue_add_tools_is_the_grantable_list() {
    let (factory, specs) = recording_factory();
    let workers = InProcessWorkers::new(factory, 2);
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());
    let continue_tool = tool_by_name(&tools, "worker_continue");

    let schema = match &continue_tool.declaration().kind {
        p1_contracts::DeclarationKind::Function { input_schema } => input_schema.clone(),
        other => panic!("worker_continue must be a function tool, got {other:?}"),
    };
    assert_eq!(schema["required"], serde_json::json!(["id", "message"]));
    assert_eq!(
        schema["properties"]["add_tools"]["uniqueItems"],
        serde_json::json!(true)
    );
    assert_eq!(
        schema["properties"]["add_tools"]["items"]["enum"],
        serde_json::json!(["edit", "read", "shell"])
    );
    assert!(
        continue_tool
            .declaration()
            .description
            .contains("add_tools"),
        "the description tells the model what add_tools is for"
    );

    // An unknown module names it and the valid list, and is refused before the
    // service sees anything: no child is re-assembled, none is even started here.
    let outcome = exec_tool(
        &continue_tool,
        "worker_continue",
        r#"{"id":"w1","message":"m","add_tools":["bogus"]}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Error);
    assert!(
        outcome.content.contains("`bogus`") && outcome.content.contains("edit, read, shell"),
        "an unknown module names it and the valid list: {}",
        outcome.content
    );
    assert!(
        specs.lock().unwrap().is_empty(),
        "a refused add_tools must not reach the factory"
    );
}

// ================================================================ the worker report (ADR-0050 item 6)

/// A factory whose child reports exactly this (a fixed snapshot, as a host tap
/// would have built).
fn reporting_factory(report: WorkerReport) -> AgentFactory {
    Arc::new(move |_spec: &ChildSpec| {
        Ok(ChildAgent {
            agent: build_agent(
                Arc::new(ScriptedProvider::new(vec![text_response("child answer")])),
                "child prompt",
                vec![],
            ),
            description: "route/model".to_string(),
            report: Arc::new({
                let report = report.clone();
                move || report.clone()
            }),
            regrant: None,
        })
    })
}

/// A short-handed worker's `worker_result` begins with its report — the granted
/// tools, the `finish` it reported and every call to a tool it was not given —
/// then `---`, then today's status line and final text. The report is stored in
/// the retained result, so a later `worker_result` reads the same one.
#[tokio::test(start_paused = true)]
async fn a_finished_workers_result_begins_with_its_report() {
    let workers = InProcessWorkers::new(
        reporting_factory(WorkerReport {
            tools: vec!["read".into(), "grep".into(), "finish".into()],
            finish: Some(FinishReport {
                status: "blocked".into(),
                needs: Some("edit".into()),
                summary: Some("cannot write".into()),
            }),
            missing_tool_calls: vec![("edit".into(), 2)],
        }),
        2,
    );
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());

    let id = within(workers.start(spec())).await.unwrap();
    let status = within(workers.status(&id)).await.unwrap();
    let ChildStatus::Finished(retained) = status else {
        panic!("the child finishes: {status:?}");
    };
    assert_eq!(
        retained.report.missing_tool_calls,
        vec![("edit".to_string(), 2)]
    );

    let result = exec_tool(
        &tool_by_name(&tools, "worker_result"),
        "worker_result",
        r#"{"id":"w1"}"#,
    )
    .await;
    assert_eq!(result.status, ToolStatus::Ok);
    assert_eq!(
        result.content,
        "tools: read, grep, finish\n\
         finish: blocked — needs: edit\n\
         calls to tools it was not given: edit x2\n\
         ---\n\
         Worker w1: finished\n\n\
         child answer"
    );
}

/// A worker that called `finish` with `done` and no missing calls: the third line
/// is omitted and the finish line says `done`.
#[tokio::test(start_paused = true)]
async fn a_worker_that_finished_done_has_no_missing_call_line() {
    let workers = InProcessWorkers::new(
        reporting_factory(WorkerReport {
            tools: vec!["read".into(), "finish".into()],
            finish: Some(FinishReport {
                status: "done".into(),
                needs: None,
                summary: Some("did it".into()),
            }),
            missing_tool_calls: Vec::new(),
        }),
        2,
    );
    let service: Arc<dyn WorkerService> = workers.clone();
    let tools = all(service, grantable(), environments());
    within(workers.start(spec())).await.unwrap();

    let result = exec_tool(
        &tool_by_name(&tools, "worker_result"),
        "worker_result",
        r#"{"id":"w1"}"#,
    )
    .await;
    assert_eq!(
        result.content,
        "tools: read, finish\nfinish: done\n---\nWorker w1: finished\n\nchild answer"
    );
}
