//! The two explicit resume decisions after the review of 2026-09-20, end to end
//! through the host: a changed route is rejected without touching the session
//! (issue #2, ADR-0033), and workers of the earlier process are declared gone — to
//! the user and to the model — with their ids never reused (issue #3, ADR-0034).

mod common;

use std::sync::Arc;

use common::{Harness, provider_hook_arc, run_args, write_environment};
#[cfg(feature = "delegation")]
use p1_contracts::Item;
use p1_contracts::{
    BoxFuture, CancellationToken, Origin, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_testkit::{ScriptedProvider, text_response};
use tempfile::tempdir;

/// A scripted provider that describes itself as another origin.
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

#[tokio::test]
async fn resuming_on_another_route_is_refused_and_leaves_the_session_untouched() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(environments.path(), "here", "fake", "m", &[], "prompt");
    write_environment(
        environments.path(),
        "moved",
        "elsewhere",
        "m",
        &[],
        "prompt",
    );
    let session = workspace.path().join("session.jsonl");
    let elsewhere = ScriptedProvider::new(vec![text_response("must never be asked")]);
    let hook = |first: ScriptedProvider, elsewhere: ScriptedProvider| {
        provider_hook_arc(vec![
            ("fake", Arc::new(first) as Arc<dyn Provider>),
            (
                "elsewhere",
                Arc::new(Elsewhere {
                    inner: elsewhere,
                    origin: Origin {
                        route: "another-route".into(),
                        model: "another-model".into(),
                    },
                }) as Arc<dyn Provider>,
            ),
        ])
    };

    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(hook(
        ScriptedProvider::new(vec![text_response("one")]),
        elsewhere.clone(),
    ));
    let workspace_arg = workspace.path().to_str().unwrap();
    let session_arg = session.to_str().unwrap();
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "here",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "first",
        ],
    )
    .await;
    assert_eq!(code, 0);
    let before = std::fs::read(&session).unwrap();

    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(hook(ScriptedProvider::new(Vec::new()), elsewhere.clone()));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "moved",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "--resume",
            "second",
        ],
    )
    .await;

    assert_eq!(code, 1, "stderr: {}", harness.stderr.text());
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("recorded on fake-route/fake-model")
            && stderr.contains("cannot continue on another-route/another-model"),
        "{stderr}"
    );
    assert!(
        elsewhere.requests().is_empty(),
        "no request on the new route"
    );
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "session untouched"
    );
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn a_resumed_parent_is_told_its_earlier_workers_are_gone_and_ids_are_not_reused() {
    use p1_testkit::{json_call, tool_call_response};

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
    let session = workspace.path().join("session.jsonl");
    let workspace_arg = workspace.path().to_str().unwrap();
    let session_arg = session.to_str().unwrap();
    let start = |call_id: &str| {
        tool_call_response(vec![json_call(
            call_id,
            "worker_start",
            r#"{"environment":"b","task":"work"}"#,
        )])
    };
    // Two or three requests, depending on whether the completion lands inside the
    // turn or after it; a spare scripted step covers both.
    let parent_script = |call_id: &str| {
        ScriptedProvider::new(vec![
            start(call_id),
            text_response("started"),
            text_response("seen"),
        ])
    };

    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-a", Arc::new(parent_script("c1")) as Arc<dyn Provider>),
        (
            "fake-b",
            Arc::new(ScriptedProvider::new(vec![text_response("done")])) as Arc<dyn Provider>,
        ),
    ]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // A new process: a new worker service that knows nothing of w1.
    let parent = parent_script("c2");
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![
        ("fake-a", Arc::new(parent.clone()) as Arc<dyn Provider>),
        (
            "fake-b",
            Arc::new(ScriptedProvider::new(vec![text_response("done again")])) as Arc<dyn Provider>,
        ),
    ]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "--resume",
            "again",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let stderr = harness.stderr.text();
    assert!(
        stderr
            .contains("resume: worker(s) w1 belonged to the earlier process and are not restored"),
        "{stderr}"
    );
    let requests = parent.requests();
    let told = requests[0].history.iter().any(|item| {
        matches!(item, Item::Inbox { text, .. }
            if text.contains("(w1) no longer exist") && text.contains("resumed in a new process"))
    });
    assert!(
        told,
        "the model's first request after the resume says so: {:#?}",
        requests[0].history
    );
    let new_worker = requests[1]
        .history
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        });
    assert!(
        new_worker
            .as_deref()
            .is_some_and(|content| content.starts_with("Started worker w2 ")),
        "the new worker must not answer to w1: {new_worker:?}"
    );
}
