//! ADR-0050 item 6 (last bullet): `worker_continue` can grant a short-handed worker
//! the tools it lacks, in place, so the repair keeps the worker's context.
//!
//! A worker granted `[read]` calls `edit` (which it does not have) and finishes
//! blocked naming it. The parent continues it with `add_tools: ["edit"]`: the worker
//! is re-assembled with `read`, `edit` and `finish`, its history still holds the first
//! turn, and its report names the new tool set. An unknown module is refused and
//! nothing reaches the worker.
//!
//! The scratch environments below use only `{{tool_names}}` in their prompts, so each
//! assembles with whatever subset the grant names. Nothing here touches the network or
//! a real credential file: the providers are the testkit's scripted fakes and the
//! "route" is the fake's own description.
#![cfg(feature = "delegation")]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{Harness, provider_hook, provider_hook_arc, run_args, write_environment};
use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, Item, Provider, ProviderError, ProviderRequest,
    ProviderStream, RouteDescription, StreamEvent,
};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A parent that can start, read and continue workers, and a scratch child whose
/// prompt names exactly the tools it is assembled with. The child's own `[[tools]]`
/// lists `read`, so the grant of `read` uses the environment's own entry while `edit`
/// falls back to the module's default face.
fn scratch_environments(root: &Path) {
    write_environment(
        root,
        "add-parent",
        "fake-parent",
        "model-parent",
        &["worker_start", "worker_result", "worker_continue"],
        "PARENT {{tool_names}}",
    );
    write_environment(
        root,
        "add-child",
        "fake-child",
        "model-child",
        &["read"],
        "CHILD {{tool_names}}",
    );
}

fn tool_names(request: &ProviderRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// Every `worker_result` content the parent's LAST request carried, in call order.
fn worker_results(requests: &[ProviderRequest]) -> Vec<String> {
    let Some(last) = requests.last() else {
        return Vec::new();
    };
    last.history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.name == "worker_result" => {
                Some(result.content.clone())
            }
            _ => None,
        })
        .collect()
}

/// The parent was shown a tool result containing `needle`, in any request's history.
fn parent_saw(requests: &[ProviderRequest], needle: &str) -> bool {
    requests.iter().any(|request| {
        request.history.iter().any(|item| match item {
            Item::ToolResult(result) => result.content.contains(needle),
            _ => false,
        })
    })
}

fn start(tools: &str) -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "c1",
        "worker_start",
        &format!(r#"{{"environment":"add-child","task":"do it","tools":{tools}}}"#),
    )])
}

fn result_waiting() -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "c2",
        "worker_result",
        r#"{"id":"w1","wait":true}"#,
    )])
}

/// The worker's first turn: it calls `edit` (not granted) and then reports itself
/// blocked naming it, so the parent has a reason to re-grant.
fn short_handed_child() -> ScriptedProvider {
    ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "e1",
            "edit",
            r#"{"file_path":"a.txt","old_string":"x","new_string":"y"}"#,
        )]),
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"blocked","summary":"cannot edit","needs":"edit"}"#,
        )]),
        text_response("child gave up"),
        // The re-granted turn.
        text_response("child edited it"),
    ])
}

/// (a) The parent repairs the worker in place: its next turn declares `read`, `edit`
/// and `finish`, its history still holds the first turn, and `worker_result`'s report
/// names the new tool set.
#[tokio::test]
async fn a_continue_can_grant_the_tool_a_blocked_worker_named() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = short_handed_child();
    let child_handle = child.clone();
    let parent = ScriptedProvider::new(vec![
        start(r#"["read"]"#),
        result_waiting(),
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"here is edit","add_tools":["edit"]}"#,
        )]),
        result_waiting(),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "add-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        parent_saw(
            &parent_handle.requests(),
            "Added tools: edit. Message sent to worker w1."
        ),
        "the parent reads the grant back: {:?}",
        parent_handle.requests()
    );

    // The worker is the SAME session: one provider served both turns (three requests
    // for the first turn's two tool calls and its answer, one for the second turn).
    let requests = child_handle.requests();
    assert_eq!(requests.len(), 4, "one turn before the repair, one after");
    assert_eq!(tool_names(&requests[0]), ["read", "finish"]);
    // The re-granted turn's tool set: the grant in order, then `finish` last.
    let repaired = requests.last().unwrap();
    assert_eq!(tool_names(repaired), ["read", "edit", "finish"]);
    assert!(
        repaired.system_prompt.contains("read, edit, finish"),
        "the prompt names the new tool set: {}",
        repaired.system_prompt
    );
    // The context survived: the first turn is still in the history, ahead of the
    // message the repair carried.
    assert_eq!(
        repaired.history.first(),
        Some(&Item::User {
            text: "do it".into()
        })
    );
    assert!(
        repaired.history.iter().any(|item| match item {
            Item::Assistant(assistant) => assistant.text() == "child gave up",
            _ => false,
        }),
        "the first turn's answer is still there: {:?}",
        repaired.history
    );
    assert_eq!(
        repaired.history.last(),
        Some(&Item::User {
            text: "here is edit".into()
        })
    );

    // The second turn's report names the tools the worker now has.
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, finish\n\
             finish: blocked — needs: edit\n\
             calls to tools it was not given: edit x1\n\
             ---\n\
             Worker w1: finished\n\n\
             child gave up"
                .to_string(),
            "tools: read, edit, finish\n\
             finish: not called\n\
             ---\n\
             Worker w1: finished\n\n\
             child edited it"
                .to_string(),
        ],
        "the second result lists the re-granted tools"
    );
}

/// A child route that CONSUMES a generated prompt-cache key (it reports
/// `CacheKeySupport::Optional`), so a re-grant's assembly shows in the key the worker
/// hands the provider.
struct Cached(ScriptedProvider);

impl Provider for Cached {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            cache_key: CacheKeySupport::Optional,
            ..self.0.describe()
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.0.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.0.stream(request, cancel)
    }
}

/// (c) A re-grant keeps the worker's cache-key ordinal, so its prompt cache routing
/// survives the repair where the provider allows it.
#[tokio::test]
async fn a_regranted_worker_keeps_its_cache_key() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let inner = ScriptedProvider::new(vec![text_response("first"), text_response("second")]);
    let child = inner.clone();
    let parent = ScriptedProvider::new(vec![
        start(r#"["read"]"#),
        result_waiting(),
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"here is edit","add_tools":["edit"]}"#,
        )]),
        result_waiting(),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-parent", Arc::new(parent) as Arc<dyn Provider>),
        (
            "fake-child",
            Arc::new(Cached(inner.clone())) as Arc<dyn Provider>,
        ),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "add-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = child.requests();
    assert_eq!(requests.len(), 2, "one request per turn");
    let first = requests[0]
        .options
        .cache_key
        .clone()
        .expect("an Optional route is handed a generated key");
    assert_eq!(
        requests[1].options.cache_key.as_deref(),
        Some(first.as_str()),
        "the re-granted turn keeps the worker's key"
    );
}

/// (b) An unknown module in `add_tools` is refused with the valid list, nothing is
/// sent to the worker, and its grant is unchanged.
#[tokio::test]
async fn an_unknown_add_tools_module_is_refused_and_nothing_is_sent() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![text_response("child done")]);
    let child_handle = child.clone();
    let parent = ScriptedProvider::new(vec![
        start(r#"["read"]"#),
        result_waiting(),
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"try it","add_tools":["bogus"]}"#,
        )]),
        tool_call_response(vec![json_call("c4", "worker_result", r#"{"id":"w1"}"#)]),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "add-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = parent_handle.requests();
    assert!(
        requests.iter().any(|request| {
            request.history.iter().any(|item| match item {
                Item::ToolResult(result) => {
                    result.name == "worker_continue"
                        && result.content.contains("`bogus`")
                        && result.content.contains("Valid tools:")
                }
                _ => false,
            })
        }),
        "the refusal names the module and the valid list: {requests:?}"
    );
    assert_eq!(
        child_handle.requests().len(),
        1,
        "a refused add_tools sends nothing to the worker"
    );
    assert_eq!(
        worker_results(&requests),
        vec![
            "tools: read, finish\nfinish: not called\n---\nWorker w1: finished\n\nchild done"
                .to_string(),
            // The refused re-grant left the worker's tools as they were.
            "tools: read, finish\nfinish: not called\n---\nWorker w1: finished\n\nchild done"
                .to_string(),
        ],
        "the worker keeps its tools"
    );
}

/// (d) A tool the worker was re-granted really runs: the child's activity records the
/// effect of the new tool from that tool, so a `shell` run AFTER the re-grant is what
/// `finish`'s verification sees, and the repair can end `done`.
#[tokio::test]
async fn a_regranted_tools_effect_reaches_finish() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    // Turn 1: no `shell`, so the worker reports itself blocked naming it. Turn 2, after
    // the re-grant: it runs a command and verifies `done` with that exact command.
    let child = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"blocked","summary":"cannot run tests","needs":"shell"}"#,
        )]),
        text_response("child gave up"),
        tool_call_response(vec![json_call("s1", "shell", r#"{"command":"true"}"#)]),
        tool_call_response(vec![json_call(
            "f2",
            "finish",
            r#"{"status":"done","summary":"ran the check","verification":["true"]}"#,
        )]),
        text_response("child verified it"),
    ]);
    let parent = ScriptedProvider::new(vec![
        start(r#"["read"]"#),
        result_waiting(),
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"here is shell","add_tools":["shell"]}"#,
        )]),
        result_waiting(),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "add-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    // An accepted `finish` is the proof: its verification matched the run of `true`
    // the re-granted `shell` made. Without the new tool's effect the call is rejected
    // and the report says `finish: not called`.
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, finish\n\
             finish: blocked — needs: shell\n\
             ---\n\
             Worker w1: finished\n\n\
             child gave up"
                .to_string(),
            "tools: read, shell, finish\n\
             finish: done — commands passed: true\n\
             ---\n\
             Worker w1: finished\n\n\
             child verified it"
                .to_string(),
        ]
    );
}

/// Two child environments on their OWN routes (`fake-child-a`, `fake-child-b`), so a
/// test can watch each worker's provider requests separately while one parent drives
/// both. The child's own `[[tools]]` lists the granted module; the module's default
/// face supplies the rest.
fn twin_environments(root: &Path) {
    write_environment(
        root,
        "twin-parent",
        "fake-parent",
        "model-parent",
        &["worker_start", "worker_result", "worker_continue"],
        "PARENT {{tool_names}}",
    );
    write_environment(
        root,
        "twin-child-a",
        "fake-child-a",
        "model-child-a",
        &["read"],
        "CHILD-A {{tool_names}}",
    );
    write_environment(
        root,
        "twin-child-b",
        "fake-child-b",
        "model-child-b",
        &["grep"],
        "CHILD-B {{tool_names}}",
    );
}

/// The parent's script, then a plain answer to every notification after it: how many
/// wake-ups the worker endings take is the host's business, not this test's.
struct Tail {
    inner: ScriptedProvider,
}

impl Provider for Tail {
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
            if self.inner.remaining_steps() > 0 {
                self.inner.stream(request, cancel).await
            } else {
                ScriptedProvider::new(vec![text_response("noted")])
                    .stream(request, cancel)
                    .await
            }
        })
    }
}

/// (e) A re-grant reassembles ONLY the worker it continues: worker A is continued
/// with `add_tools: ["edit"]` and its next request carries its grant plus the added
/// module in order, while worker B's next request still carries its original grant.
#[tokio::test]
async fn a_regrant_reassembles_only_the_continued_child() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    twin_environments(environments.path());

    let child_a = ScriptedProvider::new(vec![text_response("a first"), text_response("a second")]);
    let child_a_handle = child_a.clone();
    let child_b = ScriptedProvider::new(vec![text_response("b first"), text_response("b second")]);
    let child_b_handle = child_b.clone();
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call(
                "c1",
                "worker_start",
                r#"{"environment":"twin-child-a","task":"read it","tools":["read"]}"#,
            ),
            json_call(
                "c2",
                "worker_start",
                r#"{"environment":"twin-child-b","task":"grep it","tools":["grep"]}"#,
            ),
        ]),
        // Both first turns end before either is continued, so the continue is never
        // refused as busy and the re-grant's effect is the only difference.
        tool_call_response(vec![json_call(
            "c3",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        tool_call_response(vec![json_call(
            "c4",
            "worker_result",
            r#"{"id":"w2","wait":true}"#,
        )]),
        tool_call_response(vec![
            json_call(
                "c5",
                "worker_continue",
                r#"{"id":"w1","message":"here is edit","add_tools":["edit"]}"#,
            ),
            json_call(
                "c6",
                "worker_continue",
                r#"{"id":"w2","message":"keep going"}"#,
            ),
        ]),
        tool_call_response(vec![
            json_call("c7", "worker_result", r#"{"id":"w1","wait":true}"#),
            json_call("c8", "worker_result", r#"{"id":"w2","wait":true}"#),
        ]),
        text_response("parent done"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        (
            "fake-parent",
            Arc::new(Tail { inner: parent }) as Arc<dyn Provider>,
        ),
        ("fake-child-a", Arc::new(child_a) as Arc<dyn Provider>),
        ("fake-child-b", Arc::new(child_b) as Arc<dyn Provider>),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "twin-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        parent_saw(
            &parent_handle.requests(),
            "Added tools: edit. Message sent to worker w1."
        ),
        "the parent reads the re-grant back: {:?}",
        parent_handle.requests()
    );

    let requests_a = child_a_handle.requests();
    assert_eq!(
        requests_a.len(),
        2,
        "A runs its first turn and the re-granted one"
    );
    assert_eq!(tool_names(&requests_a[0]), ["read", "finish"]);
    assert_eq!(
        tool_names(&requests_a[1]),
        ["read", "edit", "finish"],
        "A's next request is its grant then the added module, then finish"
    );

    let requests_b = child_b_handle.requests();
    assert_eq!(requests_b.len(), 2, "B runs two of its own turns");
    assert_eq!(tool_names(&requests_b[0]), ["grep", "finish"]);
    assert_eq!(
        tool_names(&requests_b[1]),
        ["grep", "finish"],
        "B's next request is unchanged by A's re-grant"
    );
}

/// A parent that can start, continue and cancel workers, and a scratch child whose
/// prompt names exactly the tools it is assembled with.
fn busy_environments(root: &Path) {
    write_environment(
        root,
        "busy-parent",
        "fake-parent",
        "model-parent",
        &[
            "worker_start",
            "worker_result",
            "worker_continue",
            "worker_cancel",
        ],
        "PARENT {{tool_names}}",
    );
    write_environment(
        root,
        "busy-child",
        "fake-child",
        "model-child",
        &["read"],
        "CHILD {{tool_names}}",
    );
}

/// (f) `worker_continue add_tools` is refused while the child's turn is still
/// running, and the grant is unchanged afterwards: the next turn still carries the
/// old toolset.
#[tokio::test]
async fn a_continue_that_adds_tools_is_refused_while_the_turn_runs() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    busy_environments(environments.path());

    // The worker's first turn yields a delta and then waits for cancellation, so it is
    // still Running when the parent's next call arrives; the cancel ends it, and the
    // plain continue after that is the next turn.
    let child = ScriptedProvider::new(vec![
        Step::EventsThenAwaitCancel(vec![StreamEvent::TextDelta {
            block: 0,
            text: "working".to_string(),
        }]),
        text_response("second turn"),
    ]);
    let child_handle = child.clone();
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"busy-child","task":"do it","tools":["read"]}"#,
        )]),
        tool_call_response(vec![json_call(
            "c2",
            "worker_continue",
            r#"{"id":"w1","message":"here is edit","add_tools":["edit"]}"#,
        )]),
        tool_call_response(vec![json_call("c3", "worker_cancel", r#"{"id":"w1"}"#)]),
        tool_call_response(vec![json_call(
            "c4",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        tool_call_response(vec![json_call(
            "c5",
            "worker_continue",
            r#"{"id":"w1","message":"again"}"#,
        )]),
        tool_call_response(vec![json_call(
            "c6",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        text_response("parent done"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        (
            "fake-parent",
            Arc::new(Tail { inner: parent }) as Arc<dyn Provider>,
        ),
        ("fake-child", Arc::new(child) as Arc<dyn Provider>),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "busy-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        parent_saw(&parent_handle.requests(), "Worker w1 is still running."),
        "the add_tools continue is refused while the turn runs: {:?}",
        parent_handle.requests()
    );

    let requests = child_handle.requests();
    assert_eq!(
        requests.len(),
        2,
        "the cancelled first turn and the turn after the refusal"
    );
    assert_eq!(tool_names(&requests[0]), ["read", "finish"]);
    assert_eq!(
        tool_names(&requests[1]),
        ["read", "finish"],
        "the refused re-grant changed nothing: the next turn still has the old toolset"
    );
    assert!(
        requests
            .iter()
            .all(|request| !tool_names(request).contains(&"edit".to_string())),
        "the refused `edit` reached no turn: {:?}",
        requests
    );
}
