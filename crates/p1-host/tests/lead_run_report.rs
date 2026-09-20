//! `scripts/run-report.py` reads the REAL journal format: this test records a session
//! through the host (real `shell`, `read`, `write` tools, scripted model), runs the
//! script on the file and checks the evidence record. It is what keeps the script
//! honest when a record shape changes. The point of the record (review 2026-09-20):
//! a non-zero shell exit is not a failed tool call, so it is counted on its own.
//!
//! A second test records a DELEGATED session: the worker writes its own
//! `FILE.w1.jsonl`, and the script discovers it and adds its tokens to the
//! parent's without changing what the parent-only fields mean.

mod common;

#[cfg(feature = "delegation")]
use common::provider_hook_arc;
use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::serde_json::{self, Value};
#[cfg(feature = "delegation")]
use p1_contracts::{
    BoxFuture, CancellationToken, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_contracts::{StopReason, StreamEvent, Usage};
use p1_testkit::{ScriptedProvider, Step, json_call, text_block, tool_call_response};
#[cfg(feature = "delegation")]
use std::sync::Arc;
use tempfile::tempdir;

fn text_with_usage(text: &str, usage: Usage) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: text.into(),
        },
        StreamEvent::Finished(p1_testkit::completed(
            vec![text_block(text)],
            StopReason::EndTurn,
            Some(usage),
        )),
    ])
}

#[tokio::test]
async fn the_report_counts_what_the_journal_holds() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read", "write", "shell"],
        "test",
    );
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("c1", "shell", r#"{"command":"echo fine"}"#),
            json_call("c2", "shell", r#"{"command":"echo broken >&2; exit 3"}"#),
            json_call("c3", "read", r#"{"file_path":"missing.txt"}"#),
            json_call("c4", "write", r#"{"file_path":"out.txt","content":"hi"}"#),
            json_call("c5", "no_such_tool", "{}"),
        ]),
        text_with_usage(
            "done",
            Usage {
                input_uncached: Some(100),
                cache_read: Some(300),
                cache_write: None,
                output: Some(40),
                reasoning_output: None,
                cost_micro_usd: None,
            },
        ),
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
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/run-report.py");
    let output = std::process::Command::new("python3")
        .arg(script)
        .arg(&session)
        .args(["--label", "unit", "--elapsed", "1.5", "--exit-code", "0"])
        .args(["--accepted", "yes", "--interventions", "0"])
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("one JSON record");

    assert_eq!(report["origin"]["route"], "fake-route");
    assert_eq!(report["requests"], 2);
    assert_eq!(report["user_inputs"], 1);
    assert_eq!(report["tool_calls"], 5);
    assert_eq!(report["tool_calls_by_status"]["ok"], 3);
    assert_eq!(report["tool_calls_by_status"]["error"], 1);
    assert_eq!(report["tool_calls_by_status"]["unavailable"], 1);
    assert_eq!(report["tool_calls_not_ok"], 2);
    // The failing command is an OK tool call — and is still visible as a failure.
    assert_eq!(report["shell_exits"]["zero"], 1);
    assert_eq!(report["shell_exits"]["non_zero"], 1);
    assert_eq!(report["tool_calls_started_without_result"], 0);
    // Usage: known parts are summed, unknown stays null, never zero.
    assert_eq!(report["usage"]["input_uncached"], 100);
    assert_eq!(report["usage"]["cache_read"], 300);
    assert_eq!(report["usage"]["cache_write"], Value::Null);
    assert_eq!(report["usage"]["cost_micro_usd"], Value::Null);
    assert_eq!(report["input_total"], 400);
    assert_eq!(report["cache_read_share"], 0.75);
    // The first response carried no usage at all.
    assert_eq!(report["responses_without_usage"], 1);
    assert_eq!(report["accepted"], "yes");
    assert_eq!(report["elapsed_seconds"], 1.5);
    assert_eq!(report["includes_worker_usage"], false);
}

/// §3b: the host's provider retries are journalled as ordinary user inputs, and the
/// report counts exactly those as `provider_retries` — a continuation or an inbox
/// message is not a retry.
#[tokio::test]
async fn the_report_counts_journalled_provider_retries() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "test",
    );
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![
        Step::SetupError(p1_contracts::ProviderError::new(
            p1_contracts::ProviderErrorKind::Transport,
            "connection reset by peer",
        )),
        p1_testkit::text_response("plain answer"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    // No test sleeps: the retry wait is injected as an already-ready future.
    harness.deps.wait = std::sync::Arc::new(|_wait| Box::pin(std::future::ready(())));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/run-report.py");
    let output = std::process::Command::new("python3")
        .arg(script)
        .arg(&session)
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("one JSON record");

    assert_eq!(report["provider_retries"], 1);
    assert_eq!(report["user_inputs"], 2, "the prompt and the retry message");
    assert_eq!(report["requests"], 2);
    assert_eq!(report["interrupted_responses"], 1);
}

/// A child provider whose response waits for `gate`: the test can hold the worker
/// running until the parent has reached a known point, so the inbox turn the
/// completion notification causes is deterministic.
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

/// The report discovers the worker's own journal, reports its numbers on their
/// own, and only then includes them in the combined totals.
#[cfg(feature = "delegation")]
#[tokio::test]
async fn the_report_reads_worker_session_files() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake-a",
        "model-a",
        &["worker_start"],
        "PARENT",
    );
    write_environment(
        environments.path(),
        "b",
        "fake-b",
        "model-b",
        &["read"],
        "CHILD",
    );

    let parent_usage = Usage {
        input_uncached: Some(1000),
        cache_read: Some(500),
        cache_write: None,
        output: Some(10),
        reasoning_output: None,
        cost_micro_usd: None,
    };
    let child_usage = Usage {
        input_uncached: Some(100),
        cache_read: Some(20),
        cache_write: Some(5),
        output: Some(7),
        reasoning_output: None,
        cost_micro_usd: Some(12_345),
    };

    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"b","task":"do it"}"#,
        )]),
        text_with_usage("parent started", parent_usage),
        p1_testkit::text_response("ack"),
        p1_testkit::text_response("ack"),
        p1_testkit::text_response("ack"),
        p1_testkit::text_response("ack"),
    ]);
    let child = ScriptedProvider::new(vec![text_with_usage("child done", child_usage)]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let gated_child = GateProvider {
        inner: child,
        gate: gate.clone(),
    };

    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-a", Arc::new(parent.clone()) as Arc<dyn Provider>),
        ("fake-b", Arc::new(gated_child) as Arc<dyn Provider>),
    ]));

    // Release the child only after the parent has ended its second request, so the
    // completion notification is the next thing the host sees.
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

    let session = workspace.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    waiter.abort();
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/run-report.py");
    let output = std::process::Command::new("python3")
        .arg(script)
        .arg(&session)
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("one JSON record");

    // The worker's own file is reported on its own terms.
    assert_eq!(report["includes_worker_usage"], true);
    let worker = &report["workers"][0];
    assert_eq!(worker["id"], "w1");
    assert_eq!(worker["origin"]["route"], "fake-route");
    assert_eq!(worker["requests"], 1);
    assert_eq!(worker["tool_calls"], 0);
    assert_eq!(worker["usage"]["input_uncached"], 100);
    assert_eq!(worker["usage"]["cache_read"], 20);
    assert_eq!(worker["usage"]["cache_write"], 5);
    assert_eq!(worker["usage"]["output"], 7);
    assert_eq!(worker["usage"]["cost_micro_usd"], 12_345);
    assert_eq!(worker["input_total"], 125);

    // The parent-only fields keep their meaning: the parent's usage alone.
    assert_eq!(report["usage"]["input_uncached"], 1000);
    assert_eq!(report["usage"]["cache_read"], 500);
    assert_eq!(report["usage"]["cache_write"], Value::Null);
    assert_eq!(report["usage"]["output"], 10);
    assert_eq!(report["usage"]["cost_micro_usd"], Value::Null);
    assert_eq!(report["input_total"], 1500);

    // Parent plus workers; a part is null only when unknown everywhere — the
    // parent's cost is unknown, the child's is not.
    assert_eq!(report["usage_with_workers"]["input_uncached"], 1100);
    assert_eq!(report["usage_with_workers"]["cache_read"], 520);
    assert_eq!(report["usage_with_workers"]["cache_write"], 5);
    assert_eq!(report["usage_with_workers"]["output"], 17);
    assert_eq!(
        report["usage_with_workers"]["reasoning_output"],
        Value::Null
    );
    assert_eq!(report["usage_with_workers"]["cost_micro_usd"], 12_345);
    assert_eq!(report["input_total_with_workers"], 1625);
}
