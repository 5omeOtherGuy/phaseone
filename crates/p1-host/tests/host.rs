//! Whole-host end-to-end tests through `HostDeps` with fake providers, fake
//! input, captured output and a `tempfile` workspace. No network, no real
//! credential file, no sleeps.

mod common;

#[cfg(feature = "delegation")]
use std::sync::Arc;

#[cfg(feature = "delegation")]
use common::provider_hook_arc;
use common::{Harness, provider_hook, run_args, shipped_environments, write_environment};
#[cfg(feature = "delegation")]
use p1_contracts::{
    BoxFuture, CancellationToken, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_contracts::{Item, StopReason, StreamEvent, Usage};
use p1_testkit::{
    ScriptedProvider, Step, json_call, text_block, text_response, tool_call_response,
};
use tempfile::tempdir;

/// The workflow tools every main agent gets after the worker tools; none when the
/// `workflows` feature is off.
#[cfg(feature = "workflows")]
const WORKFLOW_TOOLS: [&str; 4] = [
    "workflow_start",
    "workflow_status",
    "workflow_result",
    "workflow_cancel",
];
#[cfg(not(feature = "workflows"))]
const WORKFLOW_TOOLS: [&str; 0] = [];

// ------------------------------------------------------------------ (a) text

#[tokio::test]
async fn headless_text_turn_unknown_usage() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "You are a test agent.",
    );
    let provider = ScriptedProvider::new(vec![text_response("hello there")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "hi",
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert!(harness.stdout.text().contains("hello there"));
    let expected = "model fake-route/fake-model · in ? (cached ?) · out ? · cost unknown";
    assert!(
        harness.stderr.text().contains(expected),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(
        harness.stderr.text().contains(&format!("total {expected}")),
        "totals missing from stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn headless_text_turn_known_usage() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "You are a test agent.",
    );
    let usage = Usage {
        input_uncached: Some(10),
        cache_read: Some(5),
        cache_write: Some(2),
        output: Some(7),
        reasoning_output: None,
        cost_micro_usd: Some(12_300),
    };
    let outcome = p1_testkit::completed(
        vec![text_block("known usage")],
        StopReason::EndTurn,
        Some(usage),
    );
    let provider = ScriptedProvider::new(vec![Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "known usage".to_string(),
        },
        StreamEvent::Finished(outcome),
    ])]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "hi",
        ],
    )
    .await;

    assert_eq!(code, 0);
    let line = "model fake-route/fake-model · in 17 (cached 5) · out 7 · cost $0.0123";
    assert!(
        harness.stderr.text().contains(line),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(
        harness.stderr.text().contains(&format!("total {line}")),
        "totals missing: {}",
        harness.stderr.text()
    );
}

// ------------------------------------------------------------- (b) policy

#[tokio::test]
async fn headless_ask_denies_write() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read", "write"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "write",
            "{\"file_path\":\"out.txt\",\"content\":\"hi\"}",
        )]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--ask",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert!(!workspace.path().join("out.txt").exists());
    assert!(
        harness
            .stdout
            .text()
            .contains("Not permitted in headless mode with --ask."),
        "stdout: {}",
        harness.stdout.text()
    );
}

#[tokio::test]
async fn headless_ask_permits_read() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read", "write"],
        "test",
    );
    std::fs::write(workspace.path().join("input.txt"), "hello file").unwrap();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "read",
            "{\"file_path\":\"input.txt\"}",
        )]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--ask",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert!(
        handle.requests()[1].history.iter().any(
            |item| matches!(item, Item::ToolResult(result) if result.content.contains("hello file"))
        ),
        "the read call must have run"
    );
}

#[tokio::test]
async fn headless_without_flags_runs_write() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read", "write"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "write",
            "{\"file_path\":\"out.txt\",\"content\":\"hi\"}",
        )]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "hi",
        "full access is the default: no flag means no question"
    );
}

#[tokio::test]
async fn headless_with_yes_writes_the_file() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read", "write"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "write",
            "{\"file_path\":\"out.txt\",\"content\":\"hi\"}",
        )]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "hi"
    );
}

// --------------------------------------------------- (c) per-environment tools

#[tokio::test]
async fn environments_expose_only_their_own_tools_and_prompt() {
    let workspace = tempdir().unwrap();
    let claude = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"looked","verification":["none"]}"#,
        )]),
        text_response("claude done"),
    ]);
    let gpt = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"looked","verification":["none"]}"#,
        )]),
        text_response("gpt done"),
    ]);
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("anthropic-subscription", claude.clone()),
        ("openai-codex-subscription", gpt.clone()),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "claude",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0);
    let claude_request = &claude.requests()[0];
    let names: Vec<&str> = claude_request
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    // The environment's own tools, then the four worker tools the host appends to
    // every main agent (ADR-0050 item 1), then the four workflow tools (ADR-0053).
    let mut expected = vec![
        "read",
        "edit",
        "write",
        "grep",
        "shell",
        "finish",
        "worker_start",
        "worker_result",
        "worker_continue",
        "worker_cancel",
    ];
    expected.extend(WORKFLOW_TOOLS);
    assert_eq!(names, expected);
    assert!(claude_request.system_prompt.contains("`edit`"));
    assert!(!claude_request.system_prompt.contains("apply_patch"));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "gpt",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0);
    let gpt_request = &gpt.requests()[0];
    let names: Vec<&str> = gpt_request.tools.iter().map(|t| t.name.as_str()).collect();
    let mut expected = vec![
        "shell",
        "apply_patch",
        "finish",
        "worker_start",
        "worker_result",
        "worker_continue",
        "worker_cancel",
    ];
    expected.extend(WORKFLOW_TOOLS);
    assert_eq!(names, expected);
    assert!(gpt_request.system_prompt.contains("apply_patch"));
    assert!(!gpt_request.system_prompt.contains("`edit`"));
    assert!(!gpt_request.system_prompt.contains("`write`"));
}

// ---------------------------------------------------------- (d) session/resume

fn plain_environment(root: &std::path::Path) {
    write_environment(root, "plain", "fake", "fake-model", &["read"], "test");
}

#[tokio::test]
async fn session_then_resume_continues_with_dense_seq_and_history() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let session_path = workspace.path().join("session.jsonl");

    let first = ScriptedProvider::new(vec![text_response("one")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", first)]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
            "first",
        ],
    )
    .await;
    assert_eq!(code, 0);

    let second = ScriptedProvider::new(vec![text_response("two")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", second.clone())]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
            "--resume",
            "second",
        ],
    )
    .await;
    assert_eq!(code, 0);

    let loaded = p1_journal::load(&session_path).unwrap();
    assert!(loaded.truncated_tail.is_none());
    for (index, record) in loaded.records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "seq must be dense");
    }

    let request = &second.requests()[0];
    assert!(
        request
            .history
            .iter()
            .any(|item| matches!(item, Item::User { text } if text == "first"))
    );
    assert!(
        request
            .history
            .iter()
            .any(|item| matches!(item, Item::Assistant(assistant) if assistant.text() == "one"))
    );
    assert!(
        request
            .history
            .iter()
            .any(|item| matches!(item, Item::User { text } if text == "second"))
    );
}

#[tokio::test]
async fn session_without_resume_on_existing_file_is_an_error() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let session_path = workspace.path().join("session.jsonl");
    std::fs::write(&session_path, "{\"p1_journal\":1}\n").unwrap();

    let provider = ScriptedProvider::new(vec![text_response("x")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 1);
    assert!(
        harness
            .stderr
            .text()
            .contains("session file exists; pass --resume to continue it"),
        "stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn resume_reports_and_repairs_a_truncated_tail() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let session_path = workspace.path().join("session.jsonl");

    let first = ScriptedProvider::new(vec![text_response("one")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", first)]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
            "first",
        ],
    )
    .await;
    assert_eq!(code, 0);

    let tail = b"{\"seq\":999";
    let mut bytes = std::fs::read(&session_path).unwrap();
    bytes.extend_from_slice(tail);
    std::fs::write(&session_path, &bytes).unwrap();

    let second = ScriptedProvider::new(vec![text_response("two")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", second)]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
            "--resume",
            "second",
        ],
    )
    .await;

    assert_eq!(code, 0);
    let expected = format!(
        "session file had an incomplete last record ({} bytes); it was cut off",
        tail.len()
    );
    assert!(
        harness.stderr.text().contains(&expected),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(
        p1_journal::load(&session_path)
            .unwrap()
            .truncated_tail
            .is_none()
    );
}

// ------------------------------------------------------------- (e) interactive

#[tokio::test]
async fn interactive_always_answer_is_remembered() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["write"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "write",
            "{\"file_path\":\"out.txt\",\"content\":\"one\"}",
        )]),
        text_response("ok1"),
        tool_call_response(vec![json_call(
            "c2",
            "write",
            "{\"file_path\":\"out.txt\",\"content\":\"two\"}",
        )]),
        text_response("ok2"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &["go", "a", "go"]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--ask",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "two",
        "the second write must run without a second ask"
    );
}

#[tokio::test]
async fn interactive_without_flags_does_not_ask() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["write"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "write",
            "{\"file_path\":\"out.txt\",\"content\":\"one\"}",
        )]),
        text_response("ok"),
    ]);
    // Only the user's prompt: if the policy asked, it would consume this line (or
    // read EOF) instead of running the call.
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &["go"]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "one",
        "full access is the default: the call runs without a question"
    );
    assert!(
        !harness.stderr.text().contains("allow "),
        "stderr: {}",
        harness.stderr.text()
    );
}

// -------------------------------------------------------------- (f) delegation

#[cfg(feature = "delegation")]
#[derive(Clone)]
struct GateProvider {
    inner: ScriptedProvider,
    gate: Arc<tokio::sync::Notify>,
}

#[cfg(feature = "delegation")]
impl Provider for GateProvider {
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
            self.gate.notified().await;
            self.inner.stream(request, cancel).await
        })
    }
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn delegation_end_to_end_with_fakes() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake-a",
        "model-a",
        &["worker_start", "worker_result"],
        "PARENT PROMPT {{tool_names}}",
    );
    write_environment(
        environments.path(),
        "b",
        "fake-b",
        "model-b",
        &["read"],
        "CHILD PROMPT {{tool_names}}",
    );

    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            "{\"environment\":\"b\",\"task\":\"do it\",\"tools\":[\"read\"]}",
        )]),
        text_response("parent started"),
        tool_call_response(vec![json_call("c2", "worker_result", "{\"id\":\"w1\"}")]),
        text_response("parent done"),
    ]);
    let child = ScriptedProvider::new(vec![text_response("child done")]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let gated_child = GateProvider {
        inner: child.clone(),
        gate: gate.clone(),
    };

    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-a", Arc::new(parent.clone()) as Arc<dyn Provider>),
        ("fake-b", Arc::new(gated_child) as Arc<dyn Provider>),
    ]));

    // Release the child only after the parent has started its second request and
    // had time to end its turn, so the completion notification is handled by the
    // host's wait-for-inbox path.
    let waiter = {
        let parent = parent.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            while parent.requests().len() < 2 {
                tokio::task::yield_now().await;
            }
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            gate.notify_one();
        })
    };

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    waiter.abort();

    assert_eq!(code, 0);
    let parent_requests = parent.requests();
    assert_eq!(
        parent_requests.len(),
        4,
        "parent requests: {}",
        parent_requests.len()
    );
    assert!(parent_requests[0].system_prompt.contains("PARENT PROMPT"));
    assert!(!parent_requests[0].system_prompt.contains("CHILD PROMPT"));

    let child_requests = child.requests();
    assert_eq!(child_requests.len(), 1);
    let child_names: Vec<&str> = child_requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(child_names, ["read", "finish"]);
    assert!(child_requests[0].system_prompt.contains("CHILD PROMPT"));
    assert!(!child_requests[0].system_prompt.contains("PARENT PROMPT"));

    assert!(
        parent_requests[3].history.iter().any(
            |item| matches!(item, Item::ToolResult(result) if result.content.contains("child done"))
        ),
        "worker_result must return the child's text"
    );
    assert!(
        harness.stdout.text().contains("[w1] child done"),
        "child output must carry its id prefix; stdout: {}",
        harness.stdout.text()
    );
}

/// Run one parent that starts a `write`-capable child, with or without `--ask`.
/// Returns the exit code, whether the child's file exists, the parent's requests
/// and the child's requests.
#[cfg(feature = "delegation")]
async fn run_worker_write(ask: bool) -> (i32, bool, Vec<ProviderRequest>, Vec<ProviderRequest>) {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "parent",
        "fake-a",
        "model-a",
        &["worker_start", "worker_result"],
        "PARENT",
    );
    write_environment(
        environments.path(),
        "child",
        "fake-b",
        "model-b",
        &["write"],
        "CHILD",
    );
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            "{\"environment\":\"child\",\"task\":\"work\",\"tools\":[\"write\"]}",
        )]),
        // `wait` makes the child finish before the parent continues, so the test
        // needs no gate and no timing assumption.
        tool_call_response(vec![json_call(
            "c2",
            "worker_result",
            "{\"id\":\"w1\",\"wait\":true}",
        )]),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let child = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "write",
            "{\"file_path\":\"out.txt\",\"content\":\"hi\"}",
        )]),
        text_response("child done"),
    ]);
    let child_handle = child.clone();
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));

    let mut args: Vec<&str> = Vec::new();
    if ask {
        args.push("--ask");
    }
    args.extend([
        "--env",
        "parent",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "go",
    ]);
    let code = run_args(&mut harness, &args).await;
    let exists = workspace.path().join("out.txt").exists();
    (
        code,
        exists,
        parent_handle.requests(),
        child_handle.requests(),
    )
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn headless_ask_blocks_worker_writes() {
    // The spec denies EVERYTHING but `ReadOnly` in headless `--ask` runs, and
    // `worker_start` has `Effect::Delegates`, so a worker cannot even start. This
    // is the strongest form the criterion's "a worker's write is denied" can take
    // without bending the spec: no worker write can happen, because no worker runs.
    let (code, exists, parent_requests, child_requests) = run_worker_write(true).await;
    assert_eq!(code, 0);
    assert!(!exists, "the worker's write must not run under --ask");
    assert!(
        parent_requests
            .iter()
            .any(|request| request.history.iter().any(|item| matches!(
                item,
                Item::ToolResult(result) if result.content.contains(p1_host::policy::HEADLESS_DENY)
            ))),
        "the parent's worker_start must be denied with the exact headless reason"
    );
    assert!(
        child_requests.is_empty(),
        "worker_start is denied under --ask headless, so the worker never runs"
    );
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn workers_run_writes_without_ask() {
    let (code, exists, _, _) = run_worker_write(false).await;
    assert_eq!(code, 0);
    assert!(exists, "without --ask the worker's write runs");
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn interactive_reports_a_running_worker_without_blocking() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake-a",
        "model-a",
        &["worker_start"],
        "PARENT PROMPT {{tool_names}}",
    );
    write_environment(
        environments.path(),
        "b",
        "fake-b",
        "model-b",
        &["read"],
        "CHILD PROMPT {{tool_names}}",
    );

    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            "{\"environment\":\"b\",\"task\":\"do it\",\"tools\":[\"read\"]}",
        )]),
        text_response("parent started"),
    ]);
    let child = ScriptedProvider::new(vec![text_response("never")]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let gated_child = GateProvider {
        inner: child,
        gate: gate.clone(),
    };

    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &["go"]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-a", Arc::new(parent) as Arc<dyn Provider>),
        ("fake-b", Arc::new(gated_child) as Arc<dyn Provider>),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert!(
        harness
            .stderr
            .text()
            .contains("(1 worker(s) still running)"),
        "stderr: {}",
        harness.stderr.text()
    );
}

// ------------------------------------------------- (g) no-default-features

#[cfg(not(feature = "delegation"))]
#[tokio::test]
async fn worker_modules_are_unknown_without_delegation() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "workers",
        "fake",
        "fake-model",
        &["worker_start"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![text_response("x")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "workers",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 1);
    assert!(
        harness.stderr.text().contains("unknown tool module"),
        "stderr: {}",
        harness.stderr.text()
    );
}

#[cfg(not(feature = "delegation"))]
#[tokio::test]
async fn plain_environment_works_without_delegation() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let provider = ScriptedProvider::new(vec![text_response("plain works")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert!(harness.stdout.text().contains("plain works"));
}

// --------------------------------------------------------------- (h) env show

#[tokio::test]
async fn env_show_claude_has_declarations_and_no_secret() {
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    common::isolated_environment(&mut harness);
    let code = run_args(&mut harness, &["env", "show", "claude"]).await;

    assert_eq!(code, 0);
    let stdout = harness.stdout.text();
    let line = stdout.lines().next().unwrap_or_default();
    assert!(
        line.starts_with("credential  "),
        "env show names the credential source first: {stdout}"
    );
    for name in ["read", "edit", "write", "grep", "shell"] {
        assert!(stdout.contains(&format!("\"{name}\"")), "missing {name}");
    }
    for secret in ["accessToken", "refreshToken", "Bearer", "sk-"] {
        assert!(!stdout.contains(secret), "environment leaked {secret}");
    }
}

// ---------------------------------------------------------- tty reasoning

#[tokio::test]
async fn reasoning_is_dimmed_only_on_a_tty() {
    let reasoning_script = || {
        ScriptedProvider::new(vec![Step::Events(vec![
            StreamEvent::ReasoningDelta {
                block: 0,
                text: "thinking".to_string(),
            },
            StreamEvent::TextDelta {
                block: 0,
                text: "answer".to_string(),
            },
            StreamEvent::Finished(p1_testkit::completed(
                vec![text_block("answer")],
                StopReason::EndTurn,
                None,
            )),
        ])])
    };

    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let workspace = tempdir().unwrap();

    let mut tty = Harness::new(vec![environments.path().to_path_buf()], &[]);
    tty.deps.stdout_is_tty = true;
    tty.deps.catalog_hook = Some(provider_hook(vec![("fake", reasoning_script())]));
    let code = run_args(
        &mut tty,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0);
    assert!(
        tty.stdout.text().contains("\x1b[2mthinking\x1b[0m"),
        "tty stdout: {}",
        tty.stdout.text()
    );

    let mut plain = Harness::new(vec![environments.path().to_path_buf()], &[]);
    plain.deps.stdout_is_tty = false;
    plain.deps.catalog_hook = Some(provider_hook(vec![("fake", reasoning_script())]));
    let code = run_args(
        &mut plain,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0);
    assert!(!plain.stdout.text().contains("\x1b[2m"));
}

// ------------------------------------------------------------- (i) usage errors
#[tokio::test]
async fn help_and_version_render_to_stdout() {
    let mut help = Harness::new(Vec::new(), &[]);
    assert_eq!(run_args(&mut help, &["--help"]).await, 0);
    assert!(help.stdout.text().contains("usage:"));

    let mut version = Harness::new(Vec::new(), &[]);
    assert_eq!(run_args(&mut version, &["--version"]).await, 0);
    // `p1 <version> (<sha> <date>)` — the binary names the commit it was built from.
    let text = version.stdout.text();
    assert!(
        text.contains(&format!("p1 {} (", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    assert!(text.trim_end().ends_with(')'), "{text}");
}

#[tokio::test]
async fn usage_errors_exit_2() {
    assert!(p1_host::cli::parse(&["--bogus".to_string()]).is_err());

    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let provider = ScriptedProvider::new(vec![]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let options = p1_host::cli::Options {
        command: p1_host::cli::Command::Run {
            prompt: Some("go".to_string()),
        },
        env: "plain".to_string(),
        env_given: true,
        model: None,
        effort: None,
        models: None,
        workspace: Some(workspace.path().to_path_buf()),
        session: None,
        resume: true,
        compact: false,
        ask: false,
        tui: false,
        sandbox: p1_host::cli::SandboxMode::Off,
        sandbox_write: Vec::new(),
        sandbox_read: Vec::new(),
        env_pass: Vec::new(),
        max_continuations: 3,
        provider_retries: 3,
        max_idle_summaries: 6,
        instructions: Vec::new(),
        skills: Vec::new(),
    };
    let code = p1_host::run::run(&mut harness.deps, options).await;

    assert_eq!(code, 2);
    assert!(
        harness
            .stderr
            .text()
            .contains("--resume requires --session"),
        "stderr: {}",
        harness.stderr.text()
    );
}

// ----------------------------------------------------------- (j) cancellation

#[tokio::test]
async fn cancellation_exits_130_and_renders_turn_finished() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let provider =
        ScriptedProvider::new(vec![Step::EventsThenHang(vec![StreamEvent::TextDelta {
            block: 0,
            text: "partial".to_string(),
        }])]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let interrupt = harness.interrupt.clone();
    let drained = provider.drained.clone();
    tokio::spawn(async move {
        drained.notified().await;
        interrupt.fire();
    });

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 130);
    assert!(
        harness.stderr.text().contains("! turn cancelled"),
        "stderr: {}",
        harness.stderr.text()
    );
}

#[cfg(feature = "delegation")]
#[tokio::test(start_paused = true)]
async fn review_interactive_idle_parent_wakes_on_child_completion() {
    struct Lines {
        first: std::sync::atomic::AtomicBool,
        idle: Arc<tokio::sync::Notify>,
        exit: Arc<tokio::sync::Notify>,
    }
    impl p1_host::LineSource for Lines {
        fn next_line<'a>(&'a self) -> BoxFuture<'a, Option<String>> {
            Box::pin(async move {
                if !self.first.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return Some("go".into());
                }
                self.idle.notify_one();
                self.exit.notified().await;
                Some("/exit".into())
            })
        }
    }
    struct Parent {
        inner: ScriptedProvider,
        woke: Arc<tokio::sync::Notify>,
    }
    impl Provider for Parent {
        fn describe(&self) -> RouteDescription {
            self.inner.describe()
        }
        fn validate(&self, r: &ProviderRequest) -> Result<(), ProviderError> {
            self.inner.validate(r)
        }
        fn stream<'a>(
            &'a self,
            r: ProviderRequest,
            c: CancellationToken,
        ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
            if self.inner.requests().len() == 2 {
                self.woke.notify_one();
            }
            self.inner.stream(r, c)
        }
    }
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake-a",
        "a",
        &["worker_start"],
        "parent",
    );
    write_environment(environments.path(), "b", "fake-b", "b", &[], "child");
    let woke = Arc::new(tokio::sync::Notify::new());
    let parent = Arc::new(Parent {
        inner: ScriptedProvider::new(vec![
            tool_call_response(vec![json_call(
                "c1",
                "worker_start",
                r#"{"environment":"b","task":"work","tools":["read"]}"#,
            )]),
            text_response("started"),
            text_response("verified"),
        ]),
        woke: woke.clone(),
    });
    let gate = Arc::new(tokio::sync::Notify::new());
    let child = Arc::new(GateProvider {
        inner: ScriptedProvider::new(vec![text_response("done")]),
        gate: gate.clone(),
    });
    let idle = Arc::new(tokio::sync::Notify::new());
    let exit = Arc::new(tokio::sync::Notify::new());
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.lines = Arc::new(Lines {
        first: std::sync::atomic::AtomicBool::new(false),
        idle: idle.clone(),
        exit: exit.clone(),
    });
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-a", parent.clone()),
        ("fake-b", child),
    ]));
    let driver = async {
        idle.notified().await;
        gate.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), woke.notified()).await;
        exit.notify_one();
        result
    };
    let args = [
        "--yes",
        "--env",
        "a",
        "--workspace",
        workspace.path().to_str().unwrap(),
    ];
    let (_, result) = tokio::join!(run_args(&mut harness, &args), driver);
    assert!(
        result.is_ok(),
        "idle parent did not wake; requests = {}",
        parent.inner.requests().len()
    );
}
