//! The existing workflow scripts of `crates/p1-workflow/tests/` — the modularity audit
//! (`audit_script.rs`), journal replay (`replay.rs`), the run shapes (`runs.rs`) and the
//! fallback chains (`fallback.rs`) — run through the LOADED decision component (S6.9) and
//! give what they give on the native decisions.
//!
//! Each case runs twice over `p1-workflow`'s own test support (compiled here by path, one
//! copy): once on a service composed as the host composes it (`InProcessWorkflows::new`, the
//! native decisions) and once on the same service with `WasmWorkflowDecisions`, the adapter
//! over the built `p1-module-workflow-decision` component. The scripts and their scripted
//! answers are those of the named tests (the audit reads `scripts/audits/modularity.rhai`
//! itself); the assertions are not repeated here — what the native run gives is the
//! expectation, compared whole: every report (value, outcome, counts, error, step lines),
//! every journal record, and every runner request and repair message.
//!
//! A case that fans out (`parallel`, `pipeline`) reaches the runner in thread order, which
//! neither way fixes: there the worker numbers the runner hands out and the step ordinals
//! are left out and the lists are compared as sets. Every other case is compared in order.
//!
//! No case sleeps or asserts on time; each runs under S0's deadlock guard.

#[path = "../../p1-workflow/tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, WasmWorkflowDecisions};
use p1_module_tests::within_deadline;
use p1_workflow::{
    InProcessWorkflows, RunId, SchemaCheck, StartRequest, StepEnd, WorkflowSettings,
};
use serde_json::{Value, json};
use support::{
    Harness, Recorder, Scratch, ScriptedRunner, TableResolver, done, done_with, request, settings,
};

// ------------------------------------------------------------------ the two ways

/// The built decision package, as `scripts/build-modules.sh` publishes it.
const DECISION_PACKAGE: (&str, &str) = ("p1-module-workflow-decision", "p1/workflow-decision");

/// The adapter over the built decision component, loaded through a release manifest.
fn loaded_decisions() -> Arc<WasmWorkflowDecisions> {
    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules");
    let (package, name) = DECISION_PACKAGE;
    let manifest_path = built.join(package).join(format!("{package}.manifest.json"));
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
            panic!(
                "the build output {} is missing ({error}): run scripts/build-modules.sh first",
                manifest_path.display()
            )
        }),
    )
    .expect("the package manifest is JSON");
    let entry = json!({
        "name": manifest["name"],
        "digest": manifest["digest"],
        "path": format!("{package}/{package}.wasm"),
        "kind": manifest["kind"],
        "world": manifest["world"],
        "protocol": manifest["protocol"],
        "capabilities": manifest["capabilities"],
        "variant": manifest["variant"],
    });
    let release = json!({ "format": "p1-release-manifest/1", "components": [entry] });
    let release = ReleaseManifest::parse(&release.to_string()).expect("release manifest");
    let module = Loader::new(release, built)
        .expect("loader")
        .load(name)
        .expect("the built decision component loads");
    Arc::new(
        WasmWorkflowDecisions::new(&module, ExecutionLimits::default())
            .expect("the decision adapter builds"),
    )
}

/// A harness of the shared support over `settings`: the native decisions as the host
/// composes them, or the loaded component.
fn harness(settings: WorkflowSettings, loaded: Option<Arc<WasmWorkflowDecisions>>) -> Harness {
    let root = Scratch::new();
    let runner = ScriptedRunner::new();
    let recorder = Arc::new(Recorder::default());
    let run_root = root.path().to_path_buf();
    let service = match loaded {
        None => InProcessWorkflows::new(
            runner.clone(),
            Arc::new(TableResolver),
            recorder.clone(),
            settings,
            run_root,
        ),
        Some(decisions) => InProcessWorkflows::with_decisions(
            runner.clone(),
            Arc::new(TableResolver),
            recorder.clone(),
            settings,
            run_root,
            decisions,
        ),
    };
    Harness {
        runner,
        recorder,
        service,
        root,
    }
}

// ------------------------------------------------------------------ the cases

/// One run of a case: what is queued on the runner before it starts, and the run itself.
struct Run {
    queue: Box<dyn Fn(&ScriptedRunner)>,
    start: StartRequest,
    /// Resumes the case's run with this index.
    resume: Option<usize>,
}

fn run(start: StartRequest) -> Run {
    Run {
        queue: Box::new(|_| {}),
        start,
        resume: None,
    }
}

fn script(text: &str) -> Run {
    run(request(text))
}

impl Run {
    fn queued(mut self, queue: impl Fn(&ScriptedRunner) + 'static) -> Self {
        self.queue = Box::new(queue);
        self
    }

    fn resuming(mut self, index: usize) -> Self {
        self.resume = Some(index);
        self
    }
}

/// Runs `runs` in order on `harness` and returns what each gave, as comparable JSON.
async fn observe(harness: &Harness, runs: &[Run], fans_out: bool) -> Vec<Value> {
    let mut ids: Vec<RunId> = Vec::new();
    let mut observed = Vec::new();
    for run in runs {
        (run.queue)(&harness.runner);
        let mut start = run.start.clone();
        start.resume_from = run.resume.map(|index| ids[index].clone());
        let id = harness.start_request(start).await;
        let report = harness.wait(&id).await;
        let mut report = serde_json::to_value(&report).expect("a report is JSON");
        // The run root differs between the two ways; nothing else may.
        report
            .as_object_mut()
            .expect("a report is an object")
            .remove("run_dir");
        let journal = harness
            .journal(&id)
            .iter()
            .map(|record| serde_json::to_value(record).expect("a record is JSON"))
            .collect();
        observed.push(json!({
            "report": report,
            "journal": Value::Array(journal),
        }));
        ids.push(id);
    }
    let requests: Vec<Value> = harness
        .runner
        .requests()
        .into_iter()
        .map(|request| {
            json!({
                "prompt": request.prompt,
                "label": request.label,
                "role": request.role,
                "model": request.model.reference,
                "attempt": request.attempt,
                "schema": request.schema,
                "tools": request.tools,
                "phase": request.phase,
            })
        })
        .collect();
    let repairs: Vec<Value> = harness
        .runner
        .repair_messages()
        .into_iter()
        .map(|(worker, message)| json!({ "worker": worker.id, "message": message }))
        .collect();
    observed.push(json!({ "requests": requests, "repairs": repairs }));
    if fans_out {
        observed.iter_mut().for_each(in_any_order);
    }
    observed
}

/// Leaves out what follows thread arrival order in a fan-out — the runner's worker numbers
/// (`w7|prompt` keeps its prompt) and the step ordinals — and sorts every list.
fn in_any_order(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Some(rest) = text.strip_prefix('w')
                && let Some((number, prompt)) = rest.split_once('|')
                && !number.is_empty()
                && number.bytes().all(|byte| byte.is_ascii_digit())
            {
                *text = format!("w?|{prompt}");
            }
        }
        Value::Array(items) => {
            items.iter_mut().for_each(in_any_order);
            items.sort_by_key(|item| item.to_string());
        }
        Value::Object(fields) => {
            fields.remove("ordinal");
            fields.values_mut().for_each(in_any_order);
        }
        _ => {}
    }
}

/// Runs `runs` on the native decisions and on the loaded component; both must give the
/// same.
async fn same_both_ways(case: &str, settings: WorkflowSettings, runs: Vec<Run>, fans_out: bool) {
    within_deadline(case, async {
        let native = harness(settings.clone(), None);
        let expected = observe(&native, &runs, fans_out).await;

        let decisions = loaded_decisions();
        let loaded = harness(settings, Some(decisions.clone()));
        let actual = observe(&loaded, &runs, fans_out).await;

        assert!(
            decisions.instances() > 0,
            "{case}: the loaded component was asked"
        );
        assert_eq!(
            actual.len(),
            expected.len(),
            "{case}: the same number of runs"
        );
        for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(
                actual, expected,
                "{case}, run {index}: the loaded component gave what the native decisions gave"
            );
        }
    })
    .await;
}

// ------------------------------------------------------------------ audit_script.rs

fn finding(id: &str, file: &str, line: u64, severity: &str) -> Value {
    json!({
        "id": id, "claim": format!("claim {id}"), "rule": "seams.md §1",
        "evidence": [{"file": file, "line": line, "quote": "let x = 1;"}],
        "repro": "rg -n x crates", "severity": severity, "fix": "remove it"
    })
}

/// The refuter prompt as the audit script builds it (see `audit_script.rs`).
fn refute_prompt(args: &Value, finding: &Value, lens: &str) -> String {
    let shown = json!({
        "id": finding["id"], "claim": finding["claim"], "rule": finding["rule"],
        "evidence": finding["evidence"], "repro": finding["repro"],
        "severity": finding["severity"], "fix": finding["fix"]
    });
    format!(
        "{}\n{}\n\n## Finding\n```json\n{}\n```\n\n## Facts\n{}",
        args["refute_preamble"].as_str().unwrap(),
        args["refuter"][lens].as_str().unwrap(),
        shown,
        args["facts"].as_str().unwrap()
    )
}

fn verdict(verdict: &str) -> StepEnd {
    done_with(
        json!({"verdict": verdict, "reason": "checked", "repro_output": "…"}),
        SchemaCheck::Passed,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_script() {
    let args = json!({
        "sha": "abc123",
        "facts": "# Facts\n- p1-core → p1-contracts",
        "units": [
            {"label": "core", "lens": "core-purity", "files": ["crates/p1-core/src/lib.rs"], "brief": "find in core"},
            {"label": "host", "lens": "composition-root", "files": ["crates/p1-host/src/run.rs"], "brief": "find in host"}
        ],
        "findings_schema": {"type": "object"},
        "verdict_schema": {"type": "object"},
        "refuter": {"rule": "RULE lens", "code": "CODE lens", "code-second": "CODE second"},
        "refute_preamble": "# Refute"
    });
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/audits/modularity.rhai"
    ))
    .expect("the audit script exists");
    let queue_args = args.clone();
    let queue = move |runner: &ScriptedRunner| {
        let args = &queue_args;
        let both = finding("f1", "crates/p1-core/src/lib.rs", 10, "high");
        let split = finding("f2", "crates/p1-core/src/lib.rs", 40, "medium");
        let low_split = finding("f3", "crates/p1-host/src/run.rs", 5, "low");
        let duplicate = finding("dup", "crates/p1-core/src/lib.rs", 12, "high");
        runner.queue(
            "find in core",
            done_with(
                json!({"unit": "core", "read": [], "findings": [both, split]}),
                SchemaCheck::Passed,
            ),
        );
        runner.queue(
            "find in host",
            done_with(
                json!({"unit": "host", "read": [], "findings": [duplicate, low_split]}),
                SchemaCheck::Passed,
            ),
        );
        let tagged = |f: &Value, unit: &str, lens: &str| {
            let mut f = f.clone();
            f["unit"] = json!(unit);
            f["lens"] = json!(lens);
            f
        };
        let f1 = tagged(&both, "core", "core-purity");
        let f2 = tagged(&split, "core", "core-purity");
        let f3 = tagged(&low_split, "host", "composition-root");
        runner.queue(&refute_prompt(args, &f1, "rule"), verdict("upheld"));
        runner.queue(&refute_prompt(args, &f1, "code"), verdict("upheld"));
        runner.queue(&refute_prompt(args, &f2, "rule"), verdict("refuted"));
        runner.queue(&refute_prompt(args, &f2, "code"), verdict("upheld"));
        runner.queue(&refute_prompt(args, &f2, "code-second"), verdict("upheld"));
        runner.queue(&refute_prompt(args, &f3, "rule"), verdict("upheld"));
        runner.queue(&refute_prompt(args, &f3, "code"), verdict("refuted"));
    };
    let mut start = request(&text);
    start.args = args;
    same_both_ways(
        "audit_script",
        WorkflowSettings::shipped(),
        vec![run(start).queued(queue)],
        true,
    )
    .await;
}

// ------------------------------------------------------------------ replay.rs

/// `replay.rs`'s five-step chain: only step 3 depends on `args.seed`.
const CHAIN: &str = r#"
    phase("replay");
    let seed = if has(args, "seed") { args.seed } else { "A" };
    let r1 = agent("step 1 (constant)", #{ label: "1" });
    let r2 = agent("step 2 (constant)", #{ label: "2" });
    let r3 = agent("step 3 for " + seed, #{ label: "3" });
    let r4 = agent("step 4 (constant)", #{ label: "4" });
    let r5 = agent("step 5 (constant)", #{ label: "5" });
    [r1.value, r2.value, r3.value, r4.value, r5.value]
"#;

fn chain(args: Value) -> Run {
    run(StartRequest {
        args,
        ..request(CHAIN)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_resumes_edits_and_rebuilds_caps() {
    same_both_ways(
        "replay_resumes_edits_and_rebuilds_caps",
        settings(),
        vec![
            chain(json!({})),
            chain(json!({})).resuming(0),
            chain(json!({"seed": "B"})).resuming(0),
        ],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_does_not_replay_a_failed_step() {
    let text = r#"[agent("steady").status, agent("flaky").status]"#;
    same_both_ways(
        "replay_does_not_replay_a_failed_step",
        settings(),
        vec![
            script(text).queued(|runner| {
                runner.queue("flaky", StepEnd::Failed("the provider went away".into()))
            }),
            script(text).resuming(0),
        ],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_matches_parallel_calls_by_content() {
    let text = r#"
        let pair = parallel([|| agent("left"), || agent("right")]);
        [agent("same").value, agent("same").value, pair[0].value, pair[1].value]
    "#;
    same_both_ways(
        "replay_matches_parallel_calls_by_content",
        settings(),
        vec![
            script(text).queued(|runner| {
                runner.queue("same", done("first answer"));
                runner.queue("same", done("second answer"));
            }),
            script(text).resuming(0),
        ],
        true,
    )
    .await;
}

// ------------------------------------------------------------------ runs.rs

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_the_review_shape() {
    same_both_ways(
        "runs_the_review_shape",
        settings(),
        vec![script(
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
        )],
        true,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_nested_parallel_in_a_stage() {
    for bound in [1, 4] {
        same_both_ways(
            "runs_nested_parallel_in_a_stage",
            WorkflowSettings {
                max_threads: bound,
                ..settings()
            },
            vec![script(
                r#"
                pipeline([1, 2, 3],
                    |x| parallel([|| agent("a" + x), || agent("b" + x)]),
                    |pair| pair.map(|e| e.value))
                "#,
            )],
            true,
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_a_cap_shared_across_roles() {
    same_both_ways(
        "runs_a_cap_shared_across_roles",
        settings(),
        vec![script(
            r#"
            [
                agent("j1", #{ role: "judge" }),
                agent("j2", #{ role: "second_judge" }),
                agent("j3", #{ role: "judge" }),
                agent("j4", #{ role: "second_judge" }),
            ]
            "#,
        )],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_schema_repairs_and_nudges() {
    let failed = |errors: &[&str]| {
        SchemaCheck::Failed(errors.iter().map(|error| error.to_string()).collect())
    };
    let without_finish = |text: &str| StepEnd::EndedWithoutFinish { text: text.into() };
    same_both_ways(
        "runs_schema_repairs_and_nudges",
        settings(),
        vec![
            // a_schema_failure_gets_one_repair
            script(
                r#"
                let schema = #{ type: "object", required: ["n"] };
                [agent("extract", #{ schema: schema }), agent("again", #{ schema: schema })]
                "#,
            )
            .queued(move |runner| {
                let errors = ["/n: expected integer", "/m: missing"];
                runner.queue("extract", done_with(json!({"n": "x"}), failed(&errors)));
                runner.queue_repair(
                    "extract",
                    done_with(json!({"n": 1, "m": 2}), SchemaCheck::Passed),
                );
                runner.queue("again", done_with(json!({"n": "x"}), failed(&errors)));
                runner.queue_repair(
                    "again",
                    done_with(json!({"n": "y"}), failed(&["/n: still wrong"])),
                );
            }),
            // a_step_that_ends_without_finish_is_nudged_once_in_the_same_worker
            script(r#"agent("answer")"#).queued(move |runner| {
                runner.queue("answer", without_finish("the answer is 4"));
                runner.queue_repair("answer", done("the answer is 4"));
            }),
            // a_worker_that_never_finishes_gets_exactly_one_nudge
            script(r#"agent("chatty")"#).queued(move |runner| {
                runner.queue("chatty", without_finish("first words"));
                runner.queue_repair("chatty", without_finish("last words"));
            }),
            // a_schema_repair_that_ends_without_finish_is_not_nudged_again
            script(r#"agent("extract once", #{ schema: #{ type: "object" } })"#).queued(
                move |runner| {
                    runner.queue(
                        "extract once",
                        done_with(json!({"n": "x"}), failed(&["/n: expected integer"])),
                    );
                    runner.queue_repair("extract once", without_finish("gave up"));
                    runner.queue_repair(
                        "extract once",
                        done_with(json!({"n": 1}), SchemaCheck::Passed),
                    );
                },
            ),
        ],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_capped_repairs_and_nudges() {
    let capped = WorkflowSettings {
        caps: BTreeMap::from([("claude-fable-5".to_string(), 1)]),
        ..settings()
    };
    // a_capped_repair_keeps_the_invalid_value
    same_both_ways(
        "runs_a_capped_repair",
        capped.clone(),
        vec![
            script(r#"agent("judge it", #{ role: "judge", schema: #{ type: "object" } })"#).queued(
                |runner| {
                    runner.queue(
                        "judge it",
                        done_with(
                            json!({"verdict": 7}),
                            SchemaCheck::Failed(vec!["/verdict: expected string".into()]),
                        ),
                    );
                },
            ),
        ],
        false,
    )
    .await;
    // a_capped_nudge_is_the_refused_repair_envelope
    same_both_ways(
        "runs_a_capped_nudge",
        capped,
        vec![
            script(r#"agent("judge it", #{ role: "judge" })"#).queued(|runner| {
                runner.queue(
                    "judge it",
                    StepEnd::EndedWithoutFinish {
                        text: "looks fine".into(),
                    },
                );
            }),
        ],
        false,
    )
    .await;
}

// ------------------------------------------------------------------ fallback.rs

/// `fallback.rs`'s three-link chain: `alpha/head → beta/second → gamma/third`.
fn chained() -> WorkflowSettings {
    let mut settings = settings();
    let worker = settings.roles.get_mut("worker").expect("the worker role");
    worker.model = "alpha/head".to_string();
    worker.fallback = vec!["beta/second".to_string(), "gamma/third".to_string()];
    settings
}

fn chained_with_caps(caps: &[(&str, u32)]) -> WorkflowSettings {
    let mut settings = chained();
    for (model, cap) in caps {
        settings.caps.insert(model.to_string(), *cap);
    }
    settings
}

fn route_failed(model: &str, error: &str) -> StepEnd {
    StepEnd::RouteFailed {
        model: model.to_string(),
        error: error.to_string(),
    }
}

const WORK: &str = r#"agent("work", #{ label: "w" })"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_walks_the_chain_on_route_failures() {
    // a_route_failure_on_the_head_hands_the_step_to_the_next_link
    same_both_ways(
        "fallback_one_hop",
        chained(),
        vec![script(WORK).queued(|runner| {
            runner.queue(
                "work",
                route_failed(
                    "alpha/head",
                    "InsufficientBalance: the account has no balance",
                ),
            );
        })],
        false,
    )
    .await;
    // a_whole_chain_failing_ends_failed_on_the_route
    same_both_ways(
        "fallback_whole_chain_fails",
        chained(),
        vec![script(WORK).queued(|runner| {
            for (model, error) in [
                ("alpha/head", "the route is unreachable"),
                ("beta/second", "the route refuses the model"),
                ("gamma/third", "InsufficientBalance: empty"),
            ] {
                runner.queue("work", route_failed(model, error));
            }
        })],
        false,
    )
    .await;
    // a_step_that_ran_and_failed_does_not_fall_back
    same_both_ways(
        "fallback_not_for_a_wrong_answer",
        chained(),
        vec![
            script(r#"[agent("work", #{ label: "w" }), agent("blocked")]"#).queued(|runner| {
                runner.queue("work", StepEnd::Failed("the answer was wrong".into()));
                runner.queue(
                    "blocked",
                    StepEnd::Blocked {
                        summary: "stuck".into(),
                        needs: "a token".into(),
                    },
                );
            }),
        ],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_skips_capped_links() {
    // a_capped_link_is_skipped_and_counted
    same_both_ways(
        "fallback_capped_link",
        chained_with_caps(&[("head", 0)]),
        vec![script(WORK)],
        false,
    )
    .await;
    // a_whole_chain_capped_ends_failed_with_the_last_cap
    same_both_ways(
        "fallback_whole_chain_capped",
        chained_with_caps(&[("head", 0), ("second", 0), ("third", 0)]),
        vec![script(WORK)],
        false,
    )
    .await;
    // a_chain_cannot_pass_a_capped_model_past_its_cap
    same_both_ways(
        "fallback_cap_not_passed",
        chained_with_caps(&[("head", 0), ("second", 1)]),
        vec![script(
            r#"[agent("one", #{ label: "a" }), agent("two", #{ label: "b" })]"#,
        )],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_repairs_stay_on_their_worker() {
    let invalid = || {
        done_with(
            json!({"n": "x"}),
            SchemaCheck::Failed(vec!["/n: expected integer".into()]),
        )
    };
    // a_schema_repair_stays_on_the_worker_that_produced_it
    same_both_ways(
        "fallback_repair_stays",
        chained(),
        vec![script(WORK).queued(move |runner| {
            runner.queue("work", invalid());
            runner.queue_repair("work", done_with(json!({"n": 1}), SchemaCheck::Passed));
        })],
        false,
    )
    .await;
    // a_repair_turn_that_loses_its_route_does_not_hop
    same_both_ways(
        "fallback_repair_loses_its_route",
        chained(),
        vec![script(WORK).queued(move |runner| {
            runner.queue("work", invalid());
            runner.queue_repair_result(
                "work",
                Ok(route_failed("alpha/head", "InsufficientBalance: empty")),
            );
        })],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_resume_replays_the_recorded_chain() {
    let one = r#"agent("work one", #{ label: "w" })"#;
    let two = r#"agent("work two", #{ label: "w" })"#;
    same_both_ways(
        "fallback_resume_replays_the_recorded_chain",
        chained(),
        vec![
            script(one).queued(|runner| {
                runner.queue(
                    "work one",
                    route_failed("alpha/head", "the route is unreachable"),
                );
            }),
            script(one).resuming(0),
            script(two)
                .queued(|runner| {
                    runner.queue(
                        "work two",
                        route_failed("alpha/head", "the route is unreachable"),
                    );
                })
                .resuming(0),
        ],
        false,
    )
    .await;
}
