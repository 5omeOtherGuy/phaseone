//! ADR-0053 item 2: steps and direct workers share ONE concurrency bound. With both
//! slots of the parent's service held by gated direct workers, a step waits for
//! capacity — no worker, no `LimitReached` reaching any model — and starts once a
//! direct worker has ended, with the next id of the shared sequence.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{provider_hook_arc, run_args};
use p1_contracts::{
    BoxFuture, CancellationToken, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use workflow_common::{Fakes, ROLES, Scratch, done, history_text, step_lines};

/// Holds every request until the test releases the gate.
struct Gated {
    inner: ScriptedProvider,
    gate: Arc<tokio::sync::Semaphore>,
}

impl Provider for Gated {
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
            let _permit = self.gate.acquire().await.expect("the gate stays open");
            self.inner.stream(request, cancel).await
        })
    }
}

/// The parent's script, then a plain answer to every notification after it: how many
/// wake-ups the three endings take is the host's business, not this test's.
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

#[tokio::test]
async fn a_step_waits_for_a_slot_held_by_a_direct_worker() {
    tokio::time::timeout(Duration::from_secs(60), async {
        capacity_body().await;
    })
    .await
    .expect("capacity workflow run hung");
}

async fn capacity_body() {
    // Steps run on `fake/other`, direct workers on the environment's default `main`.
    let scratch = Scratch::with_settings(&ROLES.replacen(
        "[workflows.roles.worker]\nmodel = \"fake/main\"",
        "[workflows.roles.worker]\nmodel = \"fake/other\"",
        1,
    ));
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call(
                "c1",
                "worker_start",
                r#"{"environment":"fake","task":"hold one","tools":["read"]}"#,
            ),
            json_call(
                "c2",
                "worker_start",
                r#"{"environment":"fake","task":"hold two","tools":["read"]}"#,
            ),
        ]),
        tool_call_response(vec![json_call(
            "c3",
            "workflow_start",
            &serde_json::json!({ "script": r#"agent("the step", #{ label: "s" })"# }).to_string(),
        )]),
        text_response("started"),
    ]);
    let fakes = Fakes::new(Vec::new(), Vec::new(), done("stepped"));
    let direct = ScriptedProvider::new(vec![
        text_response("direct done"),
        text_response("direct done"),
    ]);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let steps_before_release = Arc::new(AtomicUsize::new(usize::MAX));

    let mut harness = scratch.harness();
    let other: Arc<dyn Provider> = Arc::new(fakes.other.clone());
    let gated: Arc<dyn Provider> = Arc::new(Gated {
        inner: direct.clone(),
        gate: gate.clone(),
    });
    let parent_hook = provider_hook_arc(vec![(
        "fake-parent",
        Arc::new(Tail {
            inner: parent.clone(),
        }) as Arc<dyn Provider>,
    )]);
    harness.deps.catalog_hook = Some(Box::new(move |catalog: &mut p1_assembly::Catalog| {
        parent_hook(catalog);
        let (other, gated) = (other.clone(), gated.clone());
        catalog.provider(
            "route-fake",
            Box::new(move |spec: &p1_assembly::ProviderSpec| {
                match spec.profile.as_ref().map(|profile| profile.id.as_str()) {
                    Some("other") => Ok(other.clone()),
                    _ => Ok(gated.clone()),
                }
            }),
        );
    }));

    // Release the direct workers only once the parent's `workflow_start` has answered
    // (its third request) and the run has had every chance to start its step.
    let waiter = {
        let (parent, other, gate) = (parent.clone(), fakes.other.clone(), gate.clone());
        let seen = steps_before_release.clone();
        tokio::spawn(async move {
            while parent.requests().len() < 3 {
                tokio::task::yield_now().await;
            }
            for _ in 0..256 {
                tokio::task::yield_now().await;
            }
            seen.store(other.requests().len(), Ordering::SeqCst);
            gate.add_permits(1);
        })
    };

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
            "go",
        ],
    )
    .await;
    waiter.abort();
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "stderr: {stderr}");

    assert_eq!(
        steps_before_release.load(Ordering::SeqCst),
        0,
        "the step must not start while both slots are held"
    );
    assert_eq!(direct.requests().len(), 2, "both direct workers ran");
    assert_eq!(
        fakes.other.requests().len(),
        2,
        "the step ran after a slot freed"
    );
    let lines = step_lines(&stderr, "wf1");
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(
        lines[0].contains("s (worker → fake/other; w3) done"),
        "{lines:?}"
    );
    for request in parent.requests() {
        assert!(
            !history_text(&request).contains("workers may run at once"),
            "LimitReached reached the parent"
        );
    }
    for request in fakes.other.requests() {
        assert!(!history_text(&request).contains("workers may run at once"));
    }
}
