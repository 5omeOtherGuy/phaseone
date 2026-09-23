//! ADR-0053 item 7: the host exits while a run is in flight — its step worker stuck in
//! a provider call. The workflow service shuts down FIRST, so the run ends `cancelled`
//! with `Ended` as its journal's last record, and the step worker's turn is cancelled
//! rather than abandoned.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{provider_hook_arc, run_args};
use p1_contracts::{
    BoxFuture, CancellationToken, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_host::LineSource;
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use workflow_common::{Scratch, read_json, step_lines};

/// A step provider that answers only once cancelled, and says when it was reached.
struct Stuck {
    inner: ScriptedProvider,
    reached: Arc<tokio::sync::Notify>,
}

impl Provider for Stuck {
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
            self.reached.notify_one();
            cancel.cancelled().await;
            self.inner.stream(request, cancel).await
        })
    }
}

/// `go`, then end of input once the step worker is inside its provider call.
struct Lines {
    first: AtomicBool,
    reached: Arc<tokio::sync::Notify>,
}

impl LineSource for Lines {
    fn next_line<'a>(&'a self) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            if !self.first.swap(true, Ordering::SeqCst) {
                return Some("go".into());
            }
            self.reached.notified().await;
            None
        })
    }
}

#[tokio::test]
async fn exiting_mid_run_cancels_the_run_and_its_worker() {
    let scratch = Scratch::new();
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "workflow_start",
            &serde_json::json!({ "script": r#"agent("never ends", #{ label: "s" })"# }).to_string(),
        )]),
        text_response("started"),
    ]);
    let reached = Arc::new(tokio::sync::Notify::new());
    let stuck: Arc<dyn Provider> = Arc::new(Stuck {
        inner: ScriptedProvider::new(vec![text_response("too late")]),
        reached: reached.clone(),
    });

    let mut harness = scratch.harness();
    harness.deps.lines = Arc::new(Lines {
        first: AtomicBool::new(false),
        reached,
    });
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-parent", Arc::new(parent.clone()) as Arc<dyn Provider>),
        ("route-fake", stuck),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "parent",
            "--session",
            scratch.session().to_str().unwrap(),
            "--workspace",
            scratch.workspace.path().to_str().unwrap(),
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "stderr: {stderr}");

    let run_dir = scratch.run_dir("wf1");
    let result = read_json(&run_dir.join("result.json"));
    assert_eq!(result["outcome"], "cancelled", "{result}");
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
    assert_eq!(last["kind"], "ended", "{journal}");
    // The engine drops a cancelled step's future before the runner can name its
    // worker, so the line carries no id; the worker's own line says it was cancelled.
    let lines = step_lines(&stderr, "wf1");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("s (worker → fake/main) cancelled")),
        "{stderr}"
    );
    assert!(stderr.contains("[w1] ! turn cancelled"), "{stderr}");
}
