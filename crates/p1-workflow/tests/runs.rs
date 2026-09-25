//! A run end to end over the scripted runner: shapes, threads, caps, schema repair,
//! journal order, cancellation, shutdown, preflight and the envelope.

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use p1_contracts::CancellationToken;
use p1_workflow::{
    InProcessWorkflows, JournalRecord, RoleSpec, RunId, RunOutcome, RunReport, RunStatus,
    SchemaCheck, StartRequest, StepEnd, StepStatus, WorkflowError, WorkflowService,
};
use serde_json::{Value, json};
use support::{Harness, Scratch, ScriptedRunner, done_with, kinds, request, settings};

fn dispatches(records: &[JournalRecord]) -> Vec<(String, u32)> {
    records
        .iter()
        .filter_map(|record| match record {
            JournalRecord::Dispatch {
                wire_model,
                attempt,
                ..
            } => Some((wire_model.clone(), *attempt)),
            _ => None,
        })
        .collect()
}

fn ended_outcome(records: &[JournalRecord]) -> RunOutcome {
    match records.last() {
        Some(JournalRecord::Ended { outcome, .. }) => *outcome,
        other => panic!("the journal does not end with Ended: {other:?}"),
    }
}

// (1)
#[tokio::test(flavor = "multi_thread")]
async fn a_parse_error_names_line_and_column_and_creates_no_run() {
    let harness = Harness::new();
    let error = harness
        .service
        .start(request("let a = 1;\nlet b = ;\n"))
        .await
        .unwrap_err();
    match error {
        WorkflowError::Parse {
            message,
            line,
            column,
        } => {
            assert_eq!((line, column), (2, 9), "{message}");
            assert!(!message.contains("line"), "{message}");
        }
        other => panic!("expected a parse error, got {other:?}"),
    }
    assert_eq!(std::fs::read_dir(harness.root.path()).unwrap().count(), 0);
    assert!(harness.service.list().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn run_ids_continue_after_the_highest_run_directory() {
    let root = Scratch::new();
    std::fs::create_dir(root.path().join("wf7")).unwrap();
    std::fs::create_dir(root.path().join("wf2")).unwrap();
    std::fs::create_dir(root.path().join("wfx")).unwrap();
    let harness = Harness::in_root(settings(), root, ScriptedRunner::new());
    let id = harness.start("1").await;
    assert_eq!(id, RunId("wf8".into()));
    let report = harness.wait(&id).await;
    let dir = harness.root.path().join("wf8");
    assert_eq!(report.run_dir, dir);
    for file in ["script.rhai", "args.json", "journal.jsonl", "result.json"] {
        assert!(dir.join(file).is_file(), "{file}");
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("args.json"))
            .unwrap()
            .trim(),
        "{}"
    );
}

// (2)
#[tokio::test(flavor = "multi_thread")]
async fn the_review_shape_keeps_order_and_completes() {
    let harness = Harness::new();
    let report = harness
        .run(
            r#"
            phase("review");
            let reviews = parallel([
                || agent("review A", #{ role: "reviewer" }),
                || agent("review B", #{ role: "reviewer" }),
                || agent("review C", #{ role: "reviewer" }),
            ]);
            phase("fix");
            let fixed = pipeline(reviews, |r| agent("fix: " + r.value), |f| agent("verify: " + f.value));
            fixed.map(|e| e.value)
            "#,
        )
        .await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(
        report.value,
        json!(
            ["A", "B", "C"]
                .iter()
                .map(|x| format!("did: verify: did: fix: did: review {x}"))
                .collect::<Vec<_>>()
        )
    );
    let requests = harness.runner.requests();
    assert_eq!(requests.len(), 9);
    let reviews: Vec<_> = requests.iter().filter(|r| r.role == "reviewer").collect();
    assert_eq!(reviews.len(), 3);
    assert!(reviews.iter().all(|r| r.phase.as_deref() == Some("review")
        && r.tools == ["read", "grep"]
        && r.model.effort.as_deref() == Some("high")));
    assert_eq!(report.counts.steps, 9);
    assert_eq!(report.counts.done, 9);
    let written: RunReport =
        serde_json::from_slice(&std::fs::read(report.run_dir.join("result.json")).unwrap())
            .unwrap();
    assert_eq!(written, report);
    harness.recorder.run_ended.notified().await;
    assert_eq!(harness.recorder.ended.lock().unwrap().len(), 1);
    assert_eq!(harness.recorder.steps.lock().unwrap().len(), 9);
}

// (3)
#[tokio::test(flavor = "multi_thread")]
async fn nested_parallel_in_a_stage_is_bounded_and_never_deadlocks() {
    let script = r#"
        pipeline([1, 2, 3],
            |x| parallel([|| agent("a" + x), || agent("b" + x)]),
            |pair| pair.map(|e| e.value))
    "#;
    let expected = json!([
        ["did: a1", "did: b1"],
        ["did: a2", "did: b2"],
        ["did: a3", "did: b3"]
    ]);
    for bound in [1, 4] {
        let harness = Harness::with(p1_workflow::WorkflowSettings {
            max_threads: bound,
            ..settings()
        });
        let report = harness.run(script).await;
        assert_eq!(
            report.outcome,
            RunOutcome::Completed,
            "bound {bound}: {report:?}"
        );
        assert_eq!(report.value, expected, "bound {bound}");
        assert_eq!(harness.runner.requests().len(), 6);
        let max = harness.runner.max_live_thunks.load(Ordering::SeqCst);
        assert!(max <= bound, "bound {bound}: {max} thunk threads at once");
    }
}

// (4)
#[tokio::test(flavor = "multi_thread")]
async fn a_cap_on_one_wire_model_is_shared_across_roles() {
    let harness = Harness::new();
    let report = harness
        .run(
            r#"
            [
                agent("j1", #{ role: "judge" }),
                agent("j2", #{ role: "second_judge" }),
                agent("j3", #{ role: "judge" }),
                agent("j4", #{ role: "second_judge" }),
            ]
            "#,
        )
        .await;
    let fourth = &report.value[3];
    assert_eq!(fourth["status"], "failed");
    assert_eq!(
        fourth["error"],
        "quota_exceeded: claude-fable-5 used=3 limit=3"
    );
    assert_eq!(fourth["attempts"], 0);
    assert_eq!(harness.runner.requests().len(), 3);
    let records = harness.journal(&report.id);
    assert_eq!(dispatches(&records).len(), 3);
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::Capped { wire_model, used: 3, limit: 3, .. } if wire_model == "claude-fable-5"
    )));
    assert_eq!(report.counts.capped, 1);
    assert_eq!(report.outcome, RunOutcome::CompletedWithIssues);
}

// (5)
#[tokio::test(flavor = "multi_thread")]
async fn a_schema_failure_gets_one_repair() {
    let harness = Harness::new();
    let errors = vec![
        "/n: expected integer".to_string(),
        "/m: missing".to_string(),
    ];
    harness.runner.queue(
        "extract",
        done_with(json!({"n": "x"}), SchemaCheck::Failed(errors.clone())),
    );
    harness.runner.queue_repair(
        "extract",
        done_with(json!({"n": 1, "m": 2}), SchemaCheck::Passed),
    );
    harness.runner.queue(
        "again",
        done_with(json!({"n": "x"}), SchemaCheck::Failed(errors.clone())),
    );
    harness.runner.queue_repair(
        "again",
        done_with(
            json!({"n": "y"}),
            SchemaCheck::Failed(vec!["/n: still wrong".into()]),
        ),
    );
    let report = harness
        .run(
            r#"
            let schema = #{ type: "object", required: ["n"] };
            [agent("extract", #{ schema: schema }), agent("again", #{ schema: schema })]
            "#,
        )
        .await;

    let repaired = &report.value[0];
    assert_eq!(repaired["status"], "done");
    assert_eq!(repaired["attempts"], 2);
    assert_eq!(repaired["value"], json!({"n": 1, "m": 2}));
    assert_eq!(repaired["schema"], "passed");

    let invalid = &report.value[1];
    assert_eq!(invalid["status"], "failed");
    assert_eq!(invalid["attempts"], 2);
    assert_eq!(invalid["error"], "invalid_output: /n: still wrong");
    assert_eq!(invalid["value"], json!({"n": "y"}));
    assert_eq!(invalid["schema"], json!({"failed": ["/n: still wrong"]}));

    let messages = harness.runner.repair_messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0].1,
        "Your result did not match the required schema:\n- /n: expected integer\n- /m: missing\n\
         Call finish again with a corrected \"result\" that matches the schema of the \"result\" parameter."
    );
    let requests = harness.runner.requests();
    assert_eq!(requests.len(), 2, "a repair is never a new request");
    assert_eq!(
        requests[0].schema,
        Some(json!({"type": "object", "required": ["n"]}))
    );
    assert_eq!(requests[0].attempt, 1);
    let attempts: Vec<u32> = dispatches(&harness.journal(&report.id))
        .into_iter()
        .map(|(_, attempt)| attempt)
        .collect();
    assert_eq!(attempts, [1, 2, 1, 2]);
    assert_eq!(report.counts.invalid_output, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capped_repair_keeps_the_invalid_value() {
    let harness = Harness::with(p1_workflow::WorkflowSettings {
        caps: BTreeMap::from([("claude-fable-5".to_string(), 1)]),
        ..settings()
    });
    harness.runner.queue(
        "judge it",
        done_with(
            json!({"verdict": 7}),
            SchemaCheck::Failed(vec!["/verdict: expected string".into()]),
        ),
    );
    let report = harness
        .run(r#"agent("judge it", #{ role: "judge", schema: #{ type: "object" } })"#)
        .await;
    assert_eq!(report.value["status"], "failed");
    assert_eq!(
        report.value["error"],
        "quota_exceeded: claude-fable-5 used=1 limit=1 (repair)"
    );
    assert_eq!(report.value["value"], json!({"verdict": 7}));
    assert_eq!(report.value["attempts"], 1);
    assert!(harness.runner.repair_messages().is_empty());
}

const FINISH_NUDGE: &str = "You ended your turn without calling finish. Call finish now: \
    status \"done\" with your result (and the evidence), or \"blocked\" with what you need.";

// ADR-0072
#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_ends_without_finish_is_nudged_once_in_the_same_worker() {
    let harness = Harness::new();
    harness.runner.queue(
        "answer",
        StepEnd::EndedWithoutFinish {
            text: "the answer is 4".into(),
        },
    );
    harness
        .runner
        .queue_repair("answer", support::done("the answer is 4"));
    let report = harness.run(r#"agent("answer")"#).await;
    assert_eq!(report.value["status"], "done", "{report:?}");
    assert_eq!(report.value["attempts"], 2);
    assert_eq!(report.value["value"], "the answer is 4");
    assert_eq!(report.value["error"], Value::Null);
    let messages = harness.runner.repair_messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].1, FINISH_NUDGE);
    assert!(
        messages[0].0.id.starts_with("w1|"),
        "the nudge goes to the worker that ran: {messages:?}"
    );
    assert_eq!(
        harness.runner.requests().len(),
        1,
        "a nudge is never a new worker"
    );
    let attempts: Vec<u32> = dispatches(&harness.journal(&report.id))
        .into_iter()
        .map(|(_, attempt)| attempt)
        .collect();
    assert_eq!(attempts, [1, 2]);
    assert_eq!(report.outcome, RunOutcome::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_that_never_finishes_gets_exactly_one_nudge() {
    let harness = Harness::new();
    harness.runner.queue(
        "chatty",
        StepEnd::EndedWithoutFinish {
            text: "first words".into(),
        },
    );
    harness.runner.queue_repair(
        "chatty",
        StepEnd::EndedWithoutFinish {
            text: "last words".into(),
        },
    );
    let report = harness.run(r#"agent("chatty")"#).await;
    assert_eq!(report.value["status"], "failed");
    assert_eq!(report.value["error"], "ended without finish");
    assert_eq!(report.value["value"], "last words");
    assert_eq!(report.value["attempts"], 2);
    let messages = harness.runner.repair_messages();
    assert_eq!(messages.len(), 1, "exactly one nudge: {messages:?}");
    assert_eq!(messages[0].1, FINISH_NUDGE);
    assert_eq!(harness.runner.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capped_nudge_is_the_refused_repair_envelope() {
    let harness = Harness::with(p1_workflow::WorkflowSettings {
        caps: BTreeMap::from([("claude-fable-5".to_string(), 1)]),
        ..settings()
    });
    harness.runner.queue(
        "judge it",
        StepEnd::EndedWithoutFinish {
            text: "looks fine".into(),
        },
    );
    let report = harness
        .run(r#"agent("judge it", #{ role: "judge" })"#)
        .await;
    assert_eq!(report.value["status"], "failed");
    assert_eq!(
        report.value["error"],
        "quota_exceeded: claude-fable-5 used=1 limit=1 (repair)"
    );
    assert_eq!(report.value["value"], "looks fine");
    assert_eq!(report.value["attempts"], 1);
    assert!(harness.runner.repair_messages().is_empty());
    assert_eq!(report.counts.capped, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schema_repair_that_ends_without_finish_is_not_nudged_again() {
    let harness = Harness::new();
    harness.runner.queue(
        "extract",
        done_with(
            json!({"n": "x"}),
            SchemaCheck::Failed(vec!["/n: expected integer".into()]),
        ),
    );
    harness.runner.queue_repair(
        "extract",
        StepEnd::EndedWithoutFinish {
            text: "gave up".into(),
        },
    );
    // Were a second nudge sent, it would find this and end the step `done`.
    harness
        .runner
        .queue_repair("extract", done_with(json!({"n": 1}), SchemaCheck::Passed));
    let report = harness
        .run(r#"agent("extract", #{ schema: #{ type: "object" } })"#)
        .await;
    assert_eq!(report.value["status"], "failed");
    assert_eq!(report.value["error"], "ended without finish");
    assert_eq!(report.value["value"], "gave up");
    assert_eq!(report.value["attempts"], 2);
    let messages = harness.runner.repair_messages();
    assert_eq!(
        messages.len(),
        1,
        "at most one repair turn per step: {messages:?}"
    );
    assert!(messages[0].1.starts_with("Your result did not match"));
}

// (6)
#[tokio::test(flavor = "multi_thread")]
async fn every_dispatch_is_journalled_before_the_runner_sees_it() {
    let harness = Harness::new();
    let hold = harness.runner.hold("slow");
    let id = harness.start(r#"agent("slow", #{ label: "s" })"#).await;
    hold.reached.notified().await;
    let records = harness.journal(&id);
    match records.last() {
        Some(JournalRecord::Dispatch {
            label,
            attempt: 1,
            prompt,
            opts,
            ..
        }) => {
            assert_eq!(label.as_deref(), Some("s"));
            assert_eq!(prompt, "slow");
            assert_eq!(opts, &json!({"label": "s"}));
        }
        other => panic!("the runner ran before its Dispatch line: {other:?}"),
    }
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, JournalRecord::Result { .. }))
    );
    hold.release.notify_one();
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(
        kinds(&harness.journal(&id)),
        ["started", "dispatch", "result", "ended"]
    );
}

// (11)
#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_spinning_loop() {
    let harness = Harness::new();
    let id = harness
        .start(r#"log("spinning"); let x = 0; loop { x += 1; }"#)
        .await;
    harness.recorder.logged.notified().await;
    harness.service.cancel(&id).await.unwrap();
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Cancelled);
    assert_eq!(ended_outcome(&harness.journal(&id)), RunOutcome::Cancelled);
    // Idempotent on an ended run.
    harness.service.cancel(&id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_drops_a_blocked_agent() {
    let harness = Harness::new();
    let hold = harness.runner.hold("forever");
    let id = harness
        .start(r#"let r = agent("forever"); log("after"); r"#)
        .await;
    hold.reached.notified().await;
    harness.service.cancel(&id).await.unwrap();
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Cancelled);
    assert!(
        hold.dropped.load(Ordering::SeqCst),
        "the runner's step was not dropped"
    );
    assert!(
        harness.recorder.logs.lock().unwrap().is_empty(),
        "the script went on"
    );
    let records = harness.journal(&id);
    assert_eq!(ended_outcome(&records), RunOutcome::Cancelled);
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::Result { envelope, .. } if envelope.status == StepStatus::Cancelled
    )));
}

// (12)
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_cancels_running_runs_and_refuses_afterwards() {
    let harness = Harness::new();
    let hold = harness.runner.hold("held");
    let id = harness.start(r#"agent("held")"#).await;
    hold.reached.notified().await;
    harness.service.shutdown().await;
    assert!(hold.dropped.load(Ordering::SeqCst));
    assert_eq!(ended_outcome(&harness.journal(&id)), RunOutcome::Cancelled);
    let ended = harness.recorder.ended.lock().unwrap().clone();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].outcome, RunOutcome::Cancelled);
    assert_eq!(
        harness.service.status(&id).await,
        Err(WorkflowError::ShutDown)
    );
    assert_eq!(
        harness.service.wait(&id, CancellationToken::new()).await,
        Err(WorkflowError::ShutDown)
    );
    assert_eq!(
        harness.service.cancel(&id).await,
        Err(WorkflowError::ShutDown)
    );
    assert_eq!(
        harness.service.start(request("1")).await,
        Err(WorkflowError::ShutDown)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_is_running_with_progress_and_wait_honours_its_cancel() {
    let harness = Harness::new();
    let hold = harness.runner.hold("held");
    let script = r#"
        phase("work");
        for i in 0..25 { log("line " + i); }
        agent("held")
    "#;
    let id = harness.start(script).await;
    hold.reached.notified().await;
    let RunStatus::Running(progress) = harness.service.status(&id).await.unwrap() else {
        panic!("ended early");
    };
    assert_eq!(progress.phase.as_deref(), Some("work"));
    assert_eq!(progress.steps_started, 1);
    assert_eq!(progress.steps_ended, 0);
    assert_eq!(progress.log.len(), 20);
    assert_eq!(progress.log.last().map(String::as_str), Some("line 24"));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        harness.service.wait(&id, cancel).await,
        Ok(RunStatus::Running(_))
    ));
    hold.release.notify_one();
    assert_eq!(harness.wait(&id).await.outcome, RunOutcome::Completed);
    assert_eq!(
        harness.service.status(&RunId("wf99".into())).await,
        Err(WorkflowError::UnknownRun)
    );
}

// (15)
#[tokio::test(flavor = "multi_thread")]
async fn max_steps_refuses_the_next_call() {
    let harness = Harness::with(p1_workflow::WorkflowSettings {
        max_steps: 2,
        ..settings()
    });
    let report = harness.run(r#"[agent("1"), agent("2"), agent("3")]"#).await;
    assert_eq!(report.value[2]["status"], "failed");
    assert_eq!(report.value[2]["error"], "max_steps: 2 reached");
    assert_eq!(report.value[2]["attempts"], 0);
    assert_eq!(harness.runner.requests().len(), 2);
    let records = harness.journal(&report.id);
    assert_eq!(
        kinds(&records)
            .iter()
            .filter(|kind| **kind == "result")
            .count(),
        3
    );
    assert_eq!(dispatches(&records).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_role_fails_the_step_and_an_unknown_option_fails_the_script() {
    let harness = Harness::new();
    let report = harness.run(r#"agent("x", #{ role: "poet" })"#).await;
    assert_eq!(report.value["status"], "failed");
    assert_eq!(report.value["error"], "unknown_role: poet");
    assert_eq!(report.value["attempts"], 0);
    assert!(harness.runner.requests().is_empty());

    let report = harness
        .run("let r = 1;\nagent(\"x\", #{ rol: \"worker\" })")
        .await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    let error = report.error.unwrap();
    assert!(error.contains(r#"agent: unknown option "rol""#), "{error}");
    assert!(error.ends_with("[line 2, column 1]"), "{error}");

    for (opts, needle) in [
        (r#"#{ tools: [] }"#, "non-empty array of strings"),
        (r#"#{ tools: ["finish"] }"#, "may not be granted"),
        (r#"#{ schema: "x" }"#, "must be a map"),
        (r#""worker""#, "opts must be a map"),
    ] {
        let report = harness.run(&format!(r#"agent("x", {opts})"#)).await;
        assert_eq!(report.outcome, RunOutcome::Failed, "{opts}");
        let error = report.error.unwrap();
        assert!(error.contains(needle), "{opts}: {error}");
    }
    assert!(harness.runner.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn role_models_override_the_model_and_keep_the_grant() {
    let harness = Harness::new();
    let mut start = request(
        r#"agent("judge", #{ role: "judge", tools: ["read", "grep"], workspace: "/w" }); agent("judge 2", #{ role: "judge" })"#,
    );
    start.role_models =
        BTreeMap::from([("judge".to_string(), "claude/claude-opus-5-5".to_string())]);
    let id = harness.start_request(start).await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    let requests = harness.runner.requests();
    assert_eq!(requests[0].model.wire_model, "claude-opus-5-5");
    assert_eq!(
        requests[0].tools,
        ["read", "grep"],
        "the call's own tools replace the grant"
    );
    assert_eq!(
        requests[0].workspace.as_deref(),
        Some(std::path::Path::new("/w"))
    );
    assert_eq!(requests[1].tools, ["read"], "the role keeps its grant");
    assert_eq!(report.steps[0].model, "claude/claude-opus-5-5");

    let mut start = request("1");
    start.role_models = BTreeMap::from([("judge".to_string(), "nowhere/x".to_string())]);
    match harness.service.start(start).await {
        Err(WorkflowError::Preflight(reason)) => {
            assert!(reason.contains("\"judge\""), "{reason}");
            assert!(reason.contains("unknown environment"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn preflight_refuses_an_empty_grant_bad_args_and_an_unknown_resume() {
    let mut with_empty = settings();
    with_empty.roles.insert(
        "idle".to_string(),
        RoleSpec {
            model: "claude/claude-opus-5-5".to_string(),
            fallback: Vec::new(),
            tools: Vec::new(),
        },
    );
    let harness = Harness::with(with_empty);
    match harness.service.start(request("1")).await {
        Err(WorkflowError::Preflight(reason)) => assert!(reason.contains("\"idle\""), "{reason}"),
        other => panic!("{other:?}"),
    }

    let harness = Harness::new();
    let bad_args = StartRequest {
        args: json!([1]),
        ..request("1")
    };
    assert!(matches!(
        harness.service.start(bad_args).await,
        Err(WorkflowError::Preflight(_))
    ));
    for from in ["wf1", "../wf1"] {
        let resume = StartRequest {
            resume_from: Some(RunId(from.into())),
            ..request("1")
        };
        assert!(matches!(
            harness.service.start(resume).await,
            Err(WorkflowError::Preflight(_))
        ));
    }
    assert_eq!(std::fs::read_dir(harness.root.path()).unwrap().count(), 0);
}

// (16)
#[tokio::test(flavor = "multi_thread")]
async fn every_step_end_has_its_envelope_shape() {
    let harness = Harness::new();
    harness.runner.queue(
        "structured",
        done_with(json!({"ok": true}), SchemaCheck::Passed),
    );
    harness.runner.queue(
        "blocked",
        StepEnd::Blocked {
            summary: "stuck".into(),
            needs: "a token".into(),
        },
    );
    harness.runner.queue(
        "silent",
        StepEnd::EndedWithoutFinish {
            text: "I stopped".into(),
        },
    );
    harness.runner.queue_repair(
        "silent",
        StepEnd::EndedWithoutFinish {
            text: "I stopped again".into(),
        },
    );
    harness.runner.queue(
        "unverified",
        StepEnd::Done {
            summary: "looked".into(),
            evidence: "not verified; parent verification required".into(),
            result: None,
            schema: SchemaCheck::NotRequested,
        },
    );
    harness
        .runner
        .queue_result("refused", Err("no such environment".into()));
    let report = harness
        .run(
            r#"[
                agent("summary", #{ label: "plain" }),
                agent("structured", #{ schema: #{ type: "object" } }),
                agent("blocked"),
                agent("silent"),
                agent("unverified"),
                agent("refused"),
            ]"#,
        )
        .await;
    let without_step = |index: usize| {
        let mut envelope = report.value[index].clone();
        let step = envelope.as_object_mut().unwrap().remove("step").unwrap();
        assert_eq!(step.as_str().unwrap().len(), 16);
        envelope
    };
    let worker = |n: u32, prompt: &str| format!("w{n}|{prompt} (claude/claude-opus-5-5)");
    // The chain each step walked (ADR-0054 item 4): one link here, none for a step
    // refused before dispatch.
    let walked = json!([{"model": "claude/claude-opus-5-5", "moved_on": null}]);
    assert_eq!(
        without_step(0),
        json!({"label": "plain", "status": "done", "value": "did: summary",
               "schema": "not_requested", "evidence": "commands passed: cargo test",
               "attempts": 1, "worker": worker(1, "summary"), "needs": null, "error": null,
               "models": walked})
    );
    assert_eq!(
        without_step(1),
        json!({"label": null, "status": "done", "value": {"ok": true}, "schema": "passed",
               "evidence": "commands passed: cargo test", "attempts": 1,
               "worker": worker(2, "structured"), "needs": null, "error": null,
               "models": walked})
    );
    assert_eq!(
        without_step(2),
        json!({"label": null, "status": "blocked", "value": null, "schema": "not_requested",
               "evidence": null, "attempts": 1, "worker": worker(3, "blocked"),
               "needs": "a token", "error": null, "models": walked})
    );
    assert_eq!(
        without_step(3),
        json!({"label": null, "status": "failed", "value": "I stopped again",
               "schema": "not_requested", "evidence": null, "attempts": 2,
               "worker": worker(4, "silent"), "needs": null, "error": "ended without finish",
               "models": walked})
    );
    assert_eq!(report.value[5]["status"], "failed");
    assert_eq!(report.value[5]["error"], "no such environment");
    assert_eq!(report.value[5]["attempts"], 1);
    assert_eq!(report.value[5]["worker"], Value::Null);
    assert_eq!(
        report.value[5]["models"], walked,
        "the dispatch happened, the worker did not start"
    );
    assert_eq!(report.outcome, RunOutcome::CompletedWithIssues);
    assert_eq!(report.counts.done, 3);
    assert_eq!(report.counts.not_verified, 1);
    assert_eq!(report.counts.blocked, 1);
    assert_eq!(report.counts.failed, 2);
    assert_eq!(report.steps[2].status, StepStatus::Blocked);
    assert_eq!(report.steps[1].schema, "passed");
}

// (17)
#[tokio::test(flavor = "multi_thread")]
async fn has_and_json_help_and_args_are_constant() {
    let harness = Harness::new();
    let start = StartRequest {
        args: json!({"name": "x", "n": 2}),
        ..request(
            r#"
            let r = agent("look at " + json(args));
            [has(args, "name"), has(args, "nope"),
             json(#{ b: [1, #{ d: 1, c: "x y" }], a: () }), args.n + 1]
            "#,
        )
    };
    let id = harness.start_request(start).await;
    let report = harness.wait(&id).await;
    assert_eq!(
        report.value,
        json!([true, false, r#"{"a":null,"b":[1,{"c":"x y","d":1}]}"#, 3])
    );
    assert_eq!(harness.runner.prompts(), [r#"look at {"n":2,"name":"x"}"#]);

    for script in ["args = #{};", "args += #{ m: 1 };"] {
        let report = harness.run(script).await;
        assert_eq!(report.outcome, RunOutcome::Failed, "{script}");
        let error = report.error.unwrap();
        assert!(error.contains("constant"), "{script}: {error}");
    }

    let start = StartRequest {
        args: json!({"n": 2}),
        ..request("args.n = 3;")
    };
    let id = harness.start_request(start).await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(
        report
            .error
            .unwrap()
            .contains("Cannot modify constant args")
    );

    // rhai 1.26.1 does let a script ADD a key to a constant map; that changes only the
    // script's own copy, never what the run recorded as its args.
    let start = StartRequest {
        args: json!({"n": 2}),
        ..request("args.fresh = 3; args.fresh")
    };
    let id = harness.start_request(start).await;
    let report = harness.wait(&id).await;
    assert_eq!(report.value, 3, "{report:?}");
    let recorded: Value =
        serde_json::from_slice(&std::fs::read(report.run_dir.join("args.json")).unwrap()).unwrap();
    assert_eq!(recorded, json!({"n": 2}));
    assert!(matches!(
        &harness.journal(&id)[0],
        JournalRecord::Started { args, .. } if *args == json!({"n": 2})
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_error_is_located_and_a_non_json_return_fails() {
    let harness = Harness::new();
    let report = harness.run("let a = 1;\n  a.nope()").await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(report.error.unwrap().ends_with("[line 2, column 5]"));
    let report = harness.run("|| 1").await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(report.error.unwrap().contains("not JSON"));
    let report = harness.run(r#"print("hello"); debug("dbg"); ()"#).await;
    assert_eq!(report.value, Value::Null);
    let logs = harness.recorder.logs.lock().unwrap().clone();
    assert_eq!(logs[0], "hello");
    assert!(logs[1].contains("dbg"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thunk_error_is_reported_after_all_siblings_joined() {
    let harness = Harness::new();
    let hold = harness.runner.hold("slow sibling");
    let id = harness
        .start(r#"parallel([|| agent("slow sibling"), || throw "first", || throw "second"])"#)
        .await;
    hold.reached.notified().await;
    hold.release.notify_one();
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    assert!(
        report.error.as_deref().unwrap().contains("first"),
        "{report:?}"
    );
    assert_eq!(
        report.counts.done, 1,
        "the sibling finished before the error was reported"
    );
}

// (18)
#[allow(dead_code)]
fn the_service_futures_are_send(service: &InProcessWorkflows) {
    fn assert_send<T: Send>(_: &T) {}
    let start = service.start(request("1"));
    assert_send(&start);
    let shutdown = service.shutdown();
    assert_send(&shutdown);
}
