//! ADR-0114: real run endings, injected resident readings, and isolated RSS exercise.
use super::*;
use crate::api::{StepEnd, StepOutcome};
use p1_contracts::BoxFuture;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicU64};

struct NoRunner;
impl StepRunner for NoRunner {
    fn run<'a>(
        &'a self,
        _: &'a StepRequest,
        _: CancellationToken,
    ) -> BoxFuture<'a, Result<StepOutcome, String>> {
        Box::pin(async { unreachable!("memory tests dispatch no workers") })
    }
    fn repair<'a>(
        &'a self,
        _: &'a WorkerRef,
        _: String,
        _: CancellationToken,
    ) -> BoxFuture<'a, Result<StepEnd, String>> {
        Box::pin(async { unreachable!("memory tests repair no workers") })
    }
}
#[derive(Default)]
struct Observer {
    failed_thunks: AtomicUsize,
    logs: Mutex<Vec<String>>,
}
impl WorkflowObserver for Observer {
    fn thunk_failed(&self, _: &RunId, _: &str) {
        self.failed_thunks.fetch_add(1, Ordering::SeqCst);
    }
    fn log(&self, _: &RunId, text: &str) {
        lock(&self.logs).push(text.into());
    }
}

async fn run(
    source: &str,
    reader: ResidentReader,
    budget: u64,
    observer: Arc<Observer>,
    configure: impl FnOnce(&mut Engine),
) -> RunReport {
    run_with_interval(source, reader, budget, Duration::ZERO, observer, configure)
        .await
        .0
}

async fn run_with_interval(
    source: &str,
    reader: ResidentReader,
    budget: u64,
    interval: Duration,
    observer: Arc<Observer>,
    configure: impl FnOnce(&mut Engine),
) -> (RunReport, Arc<MemoryBudget>) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "p1-memory-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir(&root).unwrap();
    let (ended, _) = watch::channel(None);
    let state = Arc::new(RunState {
        id: RunId("wf1".into()),
        run_dir: root.clone(),
        artifact_dir: root.clone(),
        _run_dir_handle: std::fs::File::open(&root).unwrap(),
        #[cfg(windows)]
        _windows_parent_pins: Vec::new(),
        resumed_from: None,
        runner: Arc::new(NoRunner),
        decisions: Arc::new(NativeDecisions),
        observer,
        roles: BTreeMap::new(),
        caps: CapCounter::new(BTreeMap::new(), BTreeMap::new()),
        max_steps: 200,
        workspace: None,
        base: None,
        token: CancellationToken::new(),
        handle: Handle::current(),
        journal: JournalWriter::create(&root.join("journal.jsonl")).unwrap(),
        journal_error: Mutex::new(None),
        replay: Mutex::new(Replay::none()),
        free_threads: AtomicUsize::new(2),
        calls: AtomicU32::new(0),
        record: Mutex::default(),
        ended,
    });
    let mut engine = sandboxed_engine(200);
    configure(&mut engine);
    let ast = compile(&engine, source).unwrap();
    let script = prepare_with_memory(engine, ast, state.clone(), reader, budget, interval);
    let memory = script.memory.clone();
    assert_eq!(
        memory.interval, interval,
        "engine-builder seam must honor the interval"
    );
    tokio::task::spawn_blocking(move || execute(script, Dynamic::UNIT))
        .await
        .unwrap();
    let report = state.ended.borrow().clone().unwrap();
    // Check persisted outcome too, not only the status watch.
    let records = crate::journal::read_journal_file(
        &std::fs::File::open(root.join("journal.jsonl")).unwrap(),
        "memory test",
    )
    .unwrap();
    assert!(
        matches!(records.last(), Some(JournalRecord::Ended { outcome, .. }) if *outcome == report.outcome)
    );
    drop(state);
    std::fs::remove_dir_all(root).unwrap();
    (report, memory)
}

fn crossing_reader(calls: Arc<AtomicUsize>) -> ResidentReader {
    Arc::new(move || {
        let call = calls.fetch_add(1, Ordering::SeqCst);
        Some(12345 + if call >= 3 { RUN_MEMORY_BUDGET + 1 } else { 0 })
    })
}
fn assert_budget_failure(report: &RunReport) {
    assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
    assert_eq!(report.error.as_deref(), Some(MEMORY_BUDGET_ERROR));
    assert_eq!(report.value, Value::Null);
}

#[test]
fn memory_budget_zero_interval_samples_every_checkpoint() {
    let calls = Arc::new(AtomicUsize::new(0));
    let memory = MemoryBudget::new(
        crossing_reader(calls.clone()),
        RUN_MEMORY_BUDGET,
        Duration::ZERO,
    );
    assert_eq!(memory.interval, Duration::ZERO);
    assert!(!memory.check(64));
    assert!(!memory.check(128));
    assert!(memory.check(192));
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn memory_budget_boundaries_ignore_sample_interval() {
    for source in [
        "pipeline([0], |i| { let s = \"\"; s.pad(1024, 'x'); s });",
        "agent(\"blocked\");",
        "agent(\"blocked\", #{label: \"b\"});",
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let sampled_calls = calls.clone();
        let (report, memory) = run_with_interval(
            source,
            Arc::new(move || {
                Some(
                    100 + if sampled_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        0
                    } else {
                        RUN_MEMORY_BUDGET + 1
                    },
                )
            }),
            RUN_MEMORY_BUDGET,
            // No elapsed u64 nanosecond value can reach this interval. Only forced
            // boundary samples can read again, independently of the real clock.
            Duration::MAX,
            Arc::new(Observer::default()),
            |_| {},
        )
        .await;
        assert_budget_failure(&report);
        assert_eq!(memory.interval, Duration::MAX);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(report.steps.is_empty());
    }
}

#[tokio::test]
async fn memory_budget_short_pipeline_thunks_cross_budget() {
    let calls = Arc::new(AtomicUsize::new(0));
    let sampled_calls = calls.clone();
    let ready = Arc::new(AtomicBool::new(false));
    let sampled_ready = ready.clone();
    let report = run(
        "let items = []; for i in 0..512 { items.push(i); } ready(); pipeline(items, |i| { let s = \"\"; s.pad(1024, 'x'); s });",
        Arc::new(move || {
            if sampled_ready.load(Ordering::SeqCst) {
                let sample = sampled_calls.fetch_add(1, Ordering::SeqCst);
                Some(100 + if sample >= 3 { RUN_MEMORY_BUDGET + 1 } else { 0 })
            } else {
                Some(100)
            }
        }),
        RUN_MEMORY_BUDGET,
        Arc::new(Observer::default()),
        move |engine| {
            engine.register_fn("ready", move || ready.store(true, Ordering::SeqCst));
        },
    ).await;
    assert_budget_failure(&report);
    assert!(calls.load(Ordering::SeqCst) >= 4);
}

#[tokio::test]
async fn memory_budget_contended_reader_cannot_hide_agent_breach() {
    let entered = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let thread_a = Arc::new(Mutex::new(None));
    let thread_b = Arc::new(Mutex::new(None));
    let block_once = Arc::new(AtomicBool::new(true));
    let grown = Arc::new(AtomicBool::new(false));
    let detected = Arc::new(AtomicBool::new(false));
    let reader: ResidentReader = {
        let (entered, resume) = (entered.clone(), resume.clone());
        let (thread_a, thread_b) = (thread_a.clone(), thread_b.clone());
        let (grown, detected) = (grown.clone(), detected.clone());
        Arc::new(move || {
            let current = std::thread::current().id();
            if *lock(&thread_a) == Some(current) && block_once.swap(false, Ordering::SeqCst) {
                entered.wait();
                resume.wait();
                return Some(100); // A's stale, below-budget reading.
            }
            if *lock(&thread_b) == Some(current) && grown.load(Ordering::SeqCst) {
                detected.store(true, Ordering::SeqCst);
                resume.wait();
                return Some(100 + RUN_MEMORY_BUDGET + 1);
            }
            Some(100)
        })
    };
    let report = run(
        r#"parallel([
            || { arm_a(); let x = 0; for i in 0..100 { x += i; } agent("a"); },
            || { wait_a(); let x = 0; for i in 0..100 { x += i; } grow(); agent("b", #{label: "b"}); release_a(); }
        ]);"#,
        reader,
        RUN_MEMORY_BUDGET,
        Arc::new(Observer::default()),
        move |engine| {
            engine.register_fn("arm_a", move || *lock(&thread_a) = Some(std::thread::current().id()));
            engine.register_fn("wait_a", move || { entered.wait(); });
            let growing = grown.clone();
            engine.register_fn("grow", move || {
                *lock(&thread_b) = Some(std::thread::current().id());
                growing.store(true, Ordering::SeqCst);
            });
            // Baseline agent returns unknown_role without dispatch. Release A even when
            // no sample detected B; repaired path releases A inside B's forced reader.
            engine.register_fn("release_a", move || {
                grown.store(false, Ordering::SeqCst);
                resume.wait();
            });
        },
    ).await;
    assert_budget_failure(&report);
    assert!(detected.load(Ordering::SeqCst));
    assert!(
        report
            .steps
            .iter()
            .all(|step| step.label.as_deref() != Some("b")),
        "B must stop before entering run.step"
    );
}

#[test]
fn memory_budget_contended_checkpoint_still_samples() {
    let entered = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let calls = Arc::new(AtomicUsize::new(0));
    let reader: ResidentReader = {
        let (entered, resume, calls) = (entered.clone(), resume.clone(), calls.clone());
        Arc::new(move || match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Some(100),
            1 => {
                entered.wait();
                resume.wait();
                Some(100)
            }
            _ => Some(100 + RUN_MEMORY_BUDGET + 1),
        })
    };
    let memory = Arc::new(MemoryBudget::new(reader, RUN_MEMORY_BUDGET, Duration::ZERO));
    let sampling = memory.clone();
    let thread_a = std::thread::spawn(move || sampling.check(64));
    entered.wait();
    let detected = memory.check(128);
    resume.wait();
    let latched = thread_a.join().unwrap();
    assert!(
        detected,
        "B's checkpoint must sample while A's reader is paused"
    );
    assert!(latched, "A's stale reading must not clear B's breach");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn memory_budget_is_uncatchable() {
    let observer = Arc::new(Observer::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let report = run(
        "try { let x = 0; for i in 0..100000 { x += i; } } catch { log(\"caught\"); } log(\"continued\"); 7",
        crossing_reader(calls.clone()),
        RUN_MEMORY_BUDGET,
        observer.clone(),
        |_| {},
    ).await;
    assert_budget_failure(&report);
    assert!(calls.load(Ordering::SeqCst) >= 4);
    assert!(lock(&observer.logs).is_empty());
}

#[tokio::test]
async fn memory_budget_stops_every_parallel_thunk() {
    let observer = Arc::new(Observer::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    // Don't sample the crossing until both thunks have entered evaluation.
    let ready = Arc::new(AtomicBool::new(false));
    let reading_ready = ready.clone();
    let reader = crossing_reader(calls.clone());
    let report = run(
        r#"try { parallel([
            || { meet(); try { let x = 0; for i in 0..100000 { x += i; } } catch { log("caught 1"); } log("continued 1"); },
            || { meet(); try { let x = 0; for i in 0..100000 { x += i; } } catch { log("caught 2"); } log("continued 2"); }
        ]); } catch { log("caught outer"); } log("continued outer");"#,
        Arc::new(move || if reading_ready.load(Ordering::SeqCst) { reader() } else { Some(12345) }),
        RUN_MEMORY_BUDGET,
        observer.clone(),
        move |engine| {
            engine.register_fn("meet", move || {
                barrier.wait();
                ready.store(true, Ordering::SeqCst);
            });
        },
    ).await;
    assert_budget_failure(&report);
    assert_eq!(observer.failed_thunks.load(Ordering::SeqCst), 2);
    assert!(lock(&observer.logs).is_empty());
}

#[tokio::test]
async fn memory_budget_allows_review_shapes_and_exact_boundary() {
    let calls = Arc::new(AtomicUsize::new(0));
    let reading_calls = calls.clone();
    let mut source = String::from("let envelope = #{value: args};");
    // Full 64 KiB envelope, repeatedly read: reads must cost no memory fuel.
    source = source.replace("args", &format!("\"{}\"", "x".repeat(64 * 1024)));
    source.push_str("for i in 0..4096 { let copy = envelope; }");
    for i in 0..11 {
        source.push_str(&format!(
            "let envelope{i} = #{{value: \"{}\"}};",
            "y".repeat(30 * 1024)
        ));
    }
    source.push_str("for i in 0..800 { let x = i; } 42");
    let report = run(
        &source,
        Arc::new(move || {
            Some(if reading_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                100
            } else {
                100 + RUN_MEMORY_BUDGET
            })
        }),
        RUN_MEMORY_BUDGET,
        Arc::new(Observer::default()),
        |_| {},
    )
    .await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.value, Value::from(42));
    assert!(
        calls.load(Ordering::SeqCst) > 1,
        "exercise sampled readings"
    );
}

#[tokio::test]
async fn memory_budget_unreadable_start_or_later_never_fails() {
    for missing_start in [true, false] {
        let calls = Arc::new(AtomicUsize::new(0));
        let report = run(
            "let x = 0; for i in 0..10000 { x += i; } x",
            Arc::new(move || {
                if !missing_start && calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Some(100)
                } else {
                    None
                }
            }),
            RUN_MEMORY_BUDGET,
            Arc::new(Observer::default()),
            |_| {},
        )
        .await;
        assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
        assert_eq!(report.value, Value::from(49_995_000));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "process-wide RSS: run alone with --ignored --test-threads=1"]
async fn memory_budget_real_linux_growth() {
    const TEST_BUDGET: u64 = 256 * 1024 * 1024;
    // At most 400 MiB retained even with the hook disabled; each value is capped at
    // 4 MiB. No other tests in this process may grow or free memory beside this run.
    let mut source = String::new();
    for i in 0..100 {
        source.push_str(&format!(
            "let s{i} = \"x\"; while s{i}.len() < 4 * 1024 * 1024 {{ s{i} += s{i}; }}"
        ));
    }
    source.push('1');
    let start = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let reader_start = start.clone();
    let reader_peak = peak.clone();
    let report = run(
        &source,
        Arc::new(move || {
            let reading = resident_memory().expect("Linux statm readable for real test");
            let _ = reader_start.compare_exchange(0, reading, Ordering::SeqCst, Ordering::SeqCst);
            reader_peak.fetch_max(reading, Ordering::SeqCst);
            Some(reading)
        }),
        TEST_BUDGET,
        Arc::new(Observer::default()),
        |_| {},
    )
    .await;
    let growth = peak.load(Ordering::SeqCst) - start.load(Ordering::SeqCst);
    println!("real Linux sampled RSS growth: {growth} bytes; budget: {TEST_BUDGET} bytes");
    assert_budget_failure(&report);
    assert!(growth > TEST_BUDGET);
    assert!(
        growth < 512 * 1024 * 1024,
        "generous allocation margin: {growth}"
    );
}

#[test]
#[ignore = "manual debug-build CPU timing; run alone with --ignored --nocapture"]
fn memory_budget_cpu_measurement() {
    let source = "let x = 0; for i in 0..1000000 { x += i % 7; } x";
    for repetition in 1..=3 {
        for checked in [false, true] {
            let mut engine = sandboxed_engine(200);
            engine.set_max_operations(2_000_000);
            let ast = compile(&engine, source).unwrap();
            let memory = MemoryBudget::new(
                Arc::new(resident_memory),
                RUN_MEMORY_BUDGET,
                Duration::from_millis(1),
            );
            let token = CancellationToken::new();
            let operations = Arc::new(AtomicU64::new(0));
            let counter = operations.clone();
            engine.on_progress(move |count| {
                counter.store(count, Ordering::Relaxed);
                if token.is_cancelled() {
                    Some(Dynamic::from("cancelled"))
                } else if checked
                    && memory.check(memory.operations.fetch_add(1, Ordering::Relaxed) + 1)
                {
                    Some(Dynamic::from(MEMORY_BUDGET_ERROR))
                } else {
                    None
                }
            });
            let start = Instant::now();
            let error = engine.eval_ast::<Dynamic>(&ast).unwrap_err();
            let elapsed = start.elapsed();
            assert!(
                matches!(*error, EvalAltResult::ErrorTooManyOperations(_)),
                "{error}"
            );
            assert_eq!(operations.load(Ordering::Relaxed), 2_000_000);
            println!(
                "cpu debug run={repetition} memory_check={checked} operations=2000000 elapsed_ns={}",
                elapsed.as_nanos()
            );
        }
    }
}
