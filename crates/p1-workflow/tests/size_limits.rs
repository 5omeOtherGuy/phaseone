//! Issue #121: rhai checks `max_string_size`, `max_array_size` and `max_map_size` against
//! the SUM over the whole value a call returns (`eval/data_check.rs`), and a native
//! function's result is checked like any other, so a completed fan-out of large envelopes
//! is ONE budget. The envelopes are the host's data, one per `agent()` call the run's
//! `max_steps` caps, so the engine sizes its data limits to that cap (engine.rs
//! `data_limits`) — and a script's OWN strings stay bounded by the same numbers.

mod support;

use p1_workflow::{RunOutcome, SchemaCheck, WorkflowSettings};
use serde_json::{Value, json};
use support::{Harness, done_with, settings};

/// A reviewer's `finish` result carrying about `bytes` of strings, as a real one would.
fn big_result(dimension: &str, bytes: usize) -> Value {
    json!({
        "dimension": dimension,
        "findings": (0..12)
            .map(|n| json!({
                "title": format!("{dimension} finding {n}"),
                "detail": "x".repeat(bytes / 12),
            }))
            .collect::<Vec<Value>>(),
    })
}

/// The shape the trial run wf2 failed on: 12 read-only reviewer steps under one
/// `parallel`, each ending `done` with ~30 KB of strings.
#[tokio::test(flavor = "multi_thread")]
async fn a_parallel_fan_out_of_large_envelopes_completes() {
    let dims = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"];
    let harness = Harness::new();
    for dim in dims {
        harness.runner.queue(
            &format!("review {dim}"),
            done_with(big_result(dim, 30_000), SchemaCheck::Passed),
        );
    }
    let report = harness
        .run(
            r#"
            let dims = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"];
            let reviews = parallel(dims.map(|d| || agent("review " + d, #{ role: "reviewer" })));
            reviews
            "#,
        )
        .await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    let envelopes = report.value.as_array().expect("an array of envelopes");
    assert_eq!(envelopes.len(), 12);
    let dimensions: Vec<&str> = envelopes
        .iter()
        .map(|envelope| {
            envelope["value"]["dimension"]
                .as_str()
                .expect("a dimension")
        })
        .collect();
    assert_eq!(dimensions, dims, "results keep input order");
    // One rhai budget covers all twelve values; their sum is what a fixed 64 KiB limit
    // refused after every step had already been paid for.
    let total: usize = envelopes
        .iter()
        .map(|envelope| serde_json::to_vec(envelope).expect("JSON").len())
        .sum();
    assert!(total > 12 * 30_000, "only {total} bytes of envelopes");
}

/// One verbose worker result is a value bigger than the old fixed 64 KiB limit, and the
/// single envelope a step hands the script still fits.
#[tokio::test(flavor = "multi_thread")]
async fn one_verbose_envelope_comes_back_whole() {
    let detail = "y".repeat(100 * 1024);
    let harness = Harness::new();
    harness.runner.queue(
        "review everything",
        done_with(json!({ "detail": detail }), SchemaCheck::Passed),
    );
    let report = harness
        .run(r#"agent("review everything", #{ role: "reviewer" }).value.detail"#)
        .await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.value.as_str().map(str::len), Some(100 * 1024));
}

/// The limit is a run's own size, not one value's: `max_steps = 4` sizes the string budget
/// to 4 × 64 KiB, so a script may build 128 KiB (more than the old fixed limit allowed)
/// and is refused — with rhai's own error — past its run's budget.
#[tokio::test(flavor = "multi_thread")]
async fn a_script_built_string_is_bounded_by_the_runs_step_cap() {
    let harness = Harness::with(WorkflowSettings {
        max_steps: 4,
        ..settings()
    });
    let allowed = harness
        .run(
            r#"
            let s = "x";
            while s.len() < 100 * 1024 { s += s; }
            s.len()
            "#,
        )
        .await;
    assert_eq!(allowed.outcome, RunOutcome::Completed, "{allowed:?}");
    assert_eq!(allowed.value, json!(128 * 1024));

    let refused = harness
        .run(
            r#"
            let s = "x";
            while s.len() < 512 * 1024 { s += s; }
            s.len()
            "#,
        )
        .await;
    assert_eq!(refused.outcome, RunOutcome::Failed, "{refused:?}");
    let error = refused.error.expect("a failure names its error");
    assert!(error.contains("Length of string too large"), "{error}");
}

/// The budget does NOT follow `max_steps` past the cap: at the default 200 steps the limits
/// are the 64 envelopes `DATA_BUDGET_ENVELOPES` allows — 4 MiB of strings, 262,144 array
/// items — and not 200 × 64 KiB and 200 × 4096, so the sandbox's ceiling stays independent
/// of the operator's step cap.
#[tokio::test(flavor = "multi_thread")]
async fn a_200_step_run_is_still_capped_at_sixty_four_envelopes() {
    let harness = Harness::new(); // max_steps = 200
    let allowed = harness
        .run(
            r#"
            let s = "x";
            while s.len() < 4 * 1024 * 1024 { s += s; }
            s.len()
            "#,
        )
        .await;
    assert_eq!(allowed.outcome, RunOutcome::Completed, "{allowed:?}");
    assert_eq!(allowed.value, json!(4 * 1024 * 1024));

    let refused = harness
        .run(
            r#"
            let s = "x";
            while s.len() < 8 * 1024 * 1024 { s += s; }
            s.len()
            "#,
        )
        .await;
    assert_eq!(refused.outcome, RunOutcome::Failed, "{refused:?}");
    let error = refused.error.expect("a failure names its error");
    assert!(
        error.contains("Length of string too large"),
        "8 MiB is past the 4 MiB cap: {error}"
    );

    // The array limit is capped the same way (the map limit shares the factor): doubling is
    // the one way to build a large array that does not re-walk it on every push.
    let array = harness
        .run("let a = [0]; while a.len() < 300000 { a += a; } a.len()")
        .await;
    assert_eq!(array.outcome, RunOutcome::Failed, "{array:?}");
    let error = array.error.expect("a failure names its error");
    assert!(
        error.contains("Size of array"),
        "300k items is past the 262,144 cap: {error}"
    );
}
