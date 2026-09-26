//! The acceptance measurements (S7.6): the rows of PLAN §10's performance table that can be
//! measured today, over the complete p1 path rather than a generic Wasmtime benchmark —
//! serialization of the call, the host adapter (`WasmTool`), the executor's queue, the
//! component call on a fresh instance, the capability imports the fixture makes, and the
//! redacting wrapper the host puts around every tool. The journal write that follows a
//! result is the same native code whether a tool is native or a module, so it is part of
//! neither side of an "added" difference.
//!
//! `scripts/bench-modules.sh --suite acceptance` runs these cases in the release profile, each
//! in a test process of its own so no case's memory is read after another's, and it owns
//! the thresholds; nothing here asserts on a time or a size. Every case is `#[ignore]`,
//! so the gate's `cargo test --workspace` never runs a benchmark. Each case prints exactly one
//! line on stdout, a JSON object whose `acceptance-row` is its row id in the script's table:
//! the measurements (statistic, value, unit), the sample count and a detail text. Where a path
//! has phases worth separating — the history transfers — the detail carries the p95 of each, so
//! a row above its target says where its time went; the row's own value and verdict stay the
//! whole path's, as PLAN §10 measures it. The timed phases hold only copies p1's own path makes:
//! a history's serialized payload is written once, into the request the module receives.
//!
//! Every measured call is checked to have produced the answer the path must produce, so a
//! broken path fails the case instead of reporting a time for something that did not work.
//!
//! PLAN §10's instance strategy holds throughout: one compiled `Component` per digest (one
//! `load` per case), a fresh instance per execution (the executor's own rule), no pooling.
//! "Added" figures compare against the native path of the same operation in the same
//! process and run: [`NativeEcho`], behind the same redacting wrapper. No case touches the
//! network or a real user directory; the only process a case starts is the p1 binary for
//! `cli-startup`, with a temporary HOME.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, DeclarationKind, Effect, Item,
    Origin, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome,
    ToolResultItem, ToolStatus,
};
use p1_module_protocol::WireItem;
use p1_module_runtime::{
    ExecutionLimits, ExitStatus, LoadedModule, ProcessEvent, Services, wasm_tool,
};
use p1_module_tests::{
    FIXTURE_NAME, FakeProcesses, ProcessRecord, Release, call, fake_processes, within_deadline,
};
use p1_redact::{MaskCounter, redacted};

/// Calls made on each path before any sample is kept: the first calls pay for lazy
/// initialisation (allocator arenas, the executor task, code pages) that a warm path does not.
const WARM_UP: usize = 20;

/// Samples per path for the no-op boundary: enough that its p95 rests on many samples.
const NOOP_SAMPLES: usize = 1000;
/// The no-op call's payload, PLAN §10's "1 KiB".
const NOOP_PAYLOAD: usize = 1024;

const MIB: usize = 1024 * 1024;
/// Samples of the 1 MiB history transfer.
const HISTORY_1M_SAMPLES: usize = 100;
/// Samples of the 32 MiB history transfer: fewer, each is a large transfer, and still
/// enough for a p95 that is not the maximum.
const HISTORY_32M_SAMPLES: usize = 40;
/// Warm-up transfers of a history; the first also checks the round trip is exact.
const HISTORY_WARM_UP: usize = 3;

/// Samples per guest mode for the cancellation p99: a p99 needs at least a hundred.
const CANCEL_SAMPLES: usize = 100;

/// Samples of the CLI start, per path.
const STARTUP_SAMPLES: usize = 30;
/// Warm-up starts: the binary's pages and the component file enter the page cache.
const STARTUP_WARM_UP: usize = 3;

/// Idle tool instances held at once; the reading is their mean.
const IDLE_INSTANCES: usize = 16;

/// PLAN §10's representative workload size.
const AGENTS: usize = 16;
/// Rounds of the workload before RSS is read: the allocator and the engine settle first.
const WORKLOAD_WARM_UP: usize = 5;
/// Rounds whose RSS is the steady state of `agents-16-rss`.
const WORKLOAD_ROUNDS: usize = 10;
/// `steady-growth` compares the maxima of this many consecutive windows of rounds.
const GROWTH_WINDOWS: usize = 3;

// ---- the cases ------------------------------------------------------------------------

/// PLAN §10 "Warm no-op boundary, 1 KiB payload": the fixture's echo mode against the same
/// echo in native code, interleaved call by call so both see the same box state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn boundary_noop_1k() {
    within_deadline("boundary-noop-1k", async {
        let release = Release::with_fixture();
        let module = load(&release);
        let (tool, _processes) = module_tool(&module);
        let native = native_tool();
        let payload = synthetic_text(NOOP_PAYLOAD, 0);
        let echo = call(&format!("echo:{payload}"));

        for _ in 0..WARM_UP {
            expect_echo(&tool.execute(&echo, context()).await, &payload);
            expect_echo(&native.execute(&echo, context()).await, &payload);
        }
        let mut module_ms = Vec::with_capacity(NOOP_SAMPLES);
        let mut native_ms = Vec::with_capacity(NOOP_SAMPLES);
        for _ in 0..NOOP_SAMPLES {
            let (outcome, took) = timed(tool.execute(&echo, context())).await;
            expect_echo(&outcome, &payload);
            module_ms.push(took);
            let (outcome, took) = timed(native.execute(&echo, context())).await;
            expect_echo(&outcome, &payload);
            native_ms.push(took);
        }
        let module_p95 = percentile(&module_ms, 95);
        let native_p95 = percentile(&native_ms, 95);
        emit(
            "boundary-noop-1k",
            vec![measurement("added-p95", module_p95 - native_p95, "ms")],
            NOOP_SAMPLES,
            format!(
                "module p95 {module_p95:.3} ms, native p95 {native_p95:.3} ms, \
                 {NOOP_PAYLOAD}-byte payload"
            ),
        );
    })
    .await;
}

/// PLAN §10 "1 MiB request/history transfer and validation".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn history_1m() {
    within_deadline(
        "history-1m",
        history_case("history-1m", MIB, HISTORY_1M_SAMPLES),
    )
    .await;
}

/// PLAN §10 "32 MiB synthetic history transfer and validation".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn history_32m() {
    within_deadline(
        "history-32m",
        history_case("history-32m", 32 * MIB, HISTORY_32M_SAMPLES),
    )
    .await;
}

/// PLAN §10 "Cancellation of guest CPU or provider wait", the guest-CPU half: a guest in an
/// endless loop that never checks (`spin`, stopped through the epoch interrupt and the fuel
/// cut) and one that checks `control.cancelled()` (`cooperative`). Each sample is the time
/// from `cancel()` to the caller holding the `cancelled` outcome, taken once the guest runs:
/// its `announce` spawn has reached the fake process service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn cancel_guest() {
    within_deadline("cancel-guest", async {
        let release = Release::with_fixture();
        let module = load(&release);
        let (tool, mut processes) = module_tool(&module);

        let mut worst: f64 = 0.0;
        let mut detail = Vec::new();
        for mode in ["spin", "cooperative"] {
            let mut samples = Vec::with_capacity(CANCEL_SAMPLES);
            for index in 0..WARM_UP + CANCEL_SAMPLES {
                let cancel = CancellationToken::new();
                let running = {
                    let tool = tool.clone();
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        tool.execute(&call(&format!("announce:{mode}")), ToolContext { cancel })
                            .await
                    })
                };
                let announced = processes.next_spawn().await;
                let started = Instant::now();
                cancel.cancel();
                let outcome = running.await.expect("the calling task");
                let took = millis(started.elapsed());
                assert_eq!(
                    outcome.status,
                    ToolStatus::Cancelled,
                    "{mode}: {}",
                    outcome.content
                );
                drop(announced);
                if index >= WARM_UP {
                    samples.push(took);
                }
            }
            let p99 = percentile(&samples, 99);
            worst = worst.max(p99);
            detail.push(format!("{mode} p99 {p99:.3} ms"));
        }
        emit(
            "cancel-guest",
            vec![measurement("p99", worst, "ms")],
            2 * CANCEL_SAMPLES,
            format!(
                "{}; the worse mode is the row's value; the provider wait has no \
                 provider component yet (S4)",
                detail.join(", ")
            ),
        );
    })
    .await;
}

/// PLAN §10 "Warm installed CLI startup": the p1 binary's own start (`--version`) against the
/// same start followed by what a module-loading host adds to it — reading the release
/// manifest, verifying and compiling the component, and assembling its tool. The binary is
/// `$P1_BIN` (an installed or staged release) or this profile's build of `p1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn cli_startup() {
    within_deadline("cli-startup", async {
        let p1 = p1_binary();
        let home = tempfile::tempdir().expect("a temporary HOME");
        let release = Release::with_fixture();

        let start_with_modules = || async {
            let started = Instant::now();
            run_version(&p1, home.path());
            let module = load(&release);
            let (tool, _processes) = module_tool(&module);
            assert_eq!(tool.declaration().name, "fixture");
            millis(started.elapsed())
        };
        let start_native = || {
            let started = Instant::now();
            run_version(&p1, home.path());
            millis(started.elapsed())
        };

        for _ in 0..STARTUP_WARM_UP {
            start_native();
            start_with_modules().await;
        }
        let mut native_ms = Vec::with_capacity(STARTUP_SAMPLES);
        let mut module_ms = Vec::with_capacity(STARTUP_SAMPLES);
        for _ in 0..STARTUP_SAMPLES {
            native_ms.push(start_native());
            module_ms.push(start_with_modules().await);
        }
        let module_p95 = percentile(&module_ms, 95);
        let native_p95 = percentile(&native_ms, 95);
        emit(
            "cli-startup",
            vec![measurement("added-p95", module_p95 - native_p95, "ms")],
            STARTUP_SAMPLES,
            format!(
                "with the fixture load p95 {module_p95:.3} ms, native start p95 \
                 {native_p95:.3} ms, binary {}",
                p1.display()
            ),
        );
    })
    .await;
}

/// PLAN §10 "Idle tool/policy instance": tools over the one compiled fixture, each with a
/// call in flight on its own live instance that waits in `process.next` — the adapter, its
/// restricted instance and one execution instance, all idle. The reading is the private
/// memory (`/proc/self/smaps_rollup`) they add, per tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn idle_tool() {
    within_deadline("idle-tool", async {
        let release = Release::with_fixture();
        let module = load(&release);
        // One tool through the whole cycle first, so one-time costs (the executor's first
        // task, allocator arenas) are not charged to the instances measured.
        let warm = park_idle(&module).await;
        warm.finish().await;

        let before = Memory::read();
        let mut parked = Vec::with_capacity(IDLE_INSTANCES);
        for _ in 0..IDLE_INSTANCES {
            parked.push(park_idle(&module).await);
        }
        let after = Memory::read();
        for idle in parked {
            idle.finish().await;
        }

        let count = IDLE_INSTANCES as f64;
        let private = kib_to_mib(after.private_kib - before.private_kib) / count;
        let rss = kib_to_mib(after.rss_kib - before.rss_kib) / count;
        emit(
            "idle-tool",
            vec![measurement("per-instance", private, "MiB")],
            IDLE_INSTANCES,
            format!(
                "private memory per idle tool, mean of {IDLE_INSTANCES}; VmRSS {rss:.3} MiB \
                 per tool; the policy components have no idle-instance adapter to measure here"
            ),
        );
    })
    .await;
}

/// PLAN §10 "Representative 16-agent workload": sixteen agents at once, each with its own
/// tool, each round making a 1 KiB no-op call and transferring a validated 1 MiB history.
/// The steady state is the peak RSS over the rounds after warm-up, since the instances a
/// round holds are gone by the time it ends. The native workload runs first and its steady RSS is the baseline; the module workload
/// then runs in the same process, where it reuses what the native one freed, so the
/// difference is what the module path needs beyond the native one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn agents_16_rss() {
    within_deadline("agents-16-rss", async {
        let history = Arc::new(synthetic_history(MIB));
        let natives: Vec<_> = (0..AGENTS).map(|_| native_tool()).collect();
        let native = rounds(&natives, &history, WORKLOAD_WARM_UP, WORKLOAD_ROUNDS).await;
        drop(natives);

        let release = Release::with_fixture();
        let module = load(&release);
        let (tools, _processes): (Vec<_>, Vec<_>) =
            (0..AGENTS).map(|_| module_tool(&module)).unzip();
        let modules = rounds(&tools, &history, WORKLOAD_WARM_UP, WORKLOAD_ROUNDS).await;

        let native_rss = max_of(native.iter().map(|sample| sample.peak_kib));
        let module_rss = max_of(modules.iter().map(|sample| sample.peak_kib));
        emit(
            "agents-16-rss",
            vec![measurement(
                "added-steady",
                kib_to_mib(module_rss - native_rss),
                "MiB",
            )],
            WORKLOAD_ROUNDS,
            format!(
                "steady RSS (VmHWM over the rounds after warm-up): module {:.1} MiB, \
                 native {:.1} MiB; \
                 {AGENTS} agents, 1 KiB no-op + 1 MiB history per agent per round",
                kib_to_mib(module_rss),
                kib_to_mib(native_rss)
            ),
        );
    })
    .await;
}

/// PLAN §10 "Repeated identical workload": the 16-agent module workload repeated after
/// warm-up. RSS, open file descriptors and threads are read after every round; a resource
/// keeps growing when the maximum of each window of rounds is above the one before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a benchmark: scripts/bench-modules.sh --suite acceptance runs it"]
async fn steady_growth() {
    within_deadline("steady-growth", async {
        let history = Arc::new(synthetic_history(MIB));
        let release = Release::with_fixture();
        let module = load(&release);
        let (tools, _processes): (Vec<_>, Vec<_>) =
            (0..AGENTS).map(|_| module_tool(&module)).unzip();
        let total = GROWTH_WINDOWS * WORKLOAD_ROUNDS;
        let samples = rounds(&tools, &history, WORKLOAD_WARM_UP, total).await;

        let windows = |read: fn(&Sample) -> i64| -> Vec<i64> {
            samples
                .chunks(WORKLOAD_ROUNDS)
                .map(|window| max_of(window.iter().map(read)))
                .collect()
        };
        let rss = windows(|sample| sample.rss_kib);
        let fds = windows(|sample| sample.fds);
        let threads = windows(|sample| sample.threads);
        let growing: Vec<&str> = [("rss", &rss), ("fds", &fds), ("threads", &threads)]
            .into_iter()
            .filter(|(_, maxima)| maxima.windows(2).all(|pair| pair[1] > pair[0]))
            .map(|(name, _)| name)
            .collect();
        let value = if growing.is_empty() {
            "none".to_owned()
        } else {
            format!("continuing ({})", growing.join(", "))
        };
        let rss_mib: Vec<String> = rss
            .iter()
            .map(|kib| format!("{:.1}", kib_to_mib(*kib)))
            .collect();
        emit(
            "steady-growth",
            vec![json!({"statistic": "growth", "value": value, "unit": ""})],
            total,
            format!(
                "window maxima over {GROWTH_WINDOWS} windows of {WORKLOAD_ROUNDS} rounds: \
                 VmRSS {} MiB, fds {fds:?}, threads {threads:?}",
                rss_mib.join("/")
            ),
        );
    })
    .await;
}

// ---- the paths ------------------------------------------------------------------------

/// The fixture, loaded (read, verified, compiled) once: the one `Component` of its digest.
fn load(release: &Release) -> LoadedModule {
    release
        .loader()
        .load(FIXTURE_NAME)
        .expect("load the fixture")
}

/// A tool over `module` with its own fake process service, wrapped as the host wraps it.
fn module_tool(module: &LoadedModule) -> (Arc<dyn Tool>, FakeProcesses) {
    let (process, processes) = fake_processes();
    let tool = wasm_tool(
        module,
        Services {
            process: Some(process),
            ..Services::default()
        },
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("the fixture is a tool");
    (tool, processes)
}

/// The native path of the fixture's echo mode: the same answer computed in the caller's
/// task, with no serialization, no executor queue and no instance.
struct NativeEcho {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl Tool for NativeEcho {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let (ToolInput::Text(raw) | ToolInput::Json(raw)) = &call.input;
            let mode = raw.lines().next().unwrap_or("").trim();
            match mode.strip_prefix("echo:") {
                Some(text) => ToolOutcome::ok(text),
                None => ToolOutcome {
                    status: ToolStatus::Error,
                    content: format!("unknown mode {mode:?}"),
                },
            }
        })
    }
}

/// [`NativeEcho`] behind the same redacting wrapper the module tool has.
fn native_tool() -> Arc<dyn Tool> {
    let tool = NativeEcho {
        declaration: ToolDeclaration {
            name: "fixture".to_owned(),
            description: "native echo".to_owned(),
            kind: DeclarationKind::Freeform { grammar: None },
        },
        identity: ToolIdentity {
            implementation: "native/echo".to_owned(),
            variant: "default".to_owned(),
        },
    };
    redacted(Arc::new(tool), &Arc::new(MaskCounter::new()))
}

fn context() -> ToolContext {
    ToolContext {
        cancel: CancellationToken::new(),
    }
}

fn expect_echo(outcome: &ToolOutcome, payload: &str) {
    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.content, payload, "the echo changed the payload");
}

// ---- history transfer -----------------------------------------------------------------

/// Transfers a synthetic history of at least `bytes` serialized bytes through the fixture
/// `samples` times and reports the p95 of the whole transfer. The row's detail carries the
/// p95 of each phase of that path, so a row above its target says where its time went.
async fn history_case(row: &'static str, bytes: usize, samples: usize) {
    let history = synthetic_history(bytes);
    let serialized = wire_text(&history).len();
    let release = Release::with_fixture();
    let module = load(&release);
    let (tool, _processes) = module_tool(&module);

    for index in 0..HISTORY_WARM_UP {
        let (back, _) = transfer(&tool, &history, serialized).await;
        if index == 0 {
            assert!(back == history, "{row}: the history came back changed");
        }
    }
    let mut taken = Vec::with_capacity(samples);
    let mut phases = Phases::default();
    for _ in 0..samples {
        let ((back, timings), took) = timed(transfer(&tool, &history, serialized)).await;
        assert_eq!(back.len(), history.len(), "{row}: items were lost");
        taken.push(took);
        phases.record(timings);
    }
    emit(
        row,
        vec![measurement("p95", percentile(&taken, 95), "ms")],
        samples,
        format!(
            "{} items, {serialized} serialized bytes, echoed through the fixture and \
             validated as protocol history items; p95 by phase: {}",
            history.len(),
            phases.detail()
        ),
    );
}

/// One transfer, as a host hands history to a module and takes it back: the native items
/// to their protocol wire form and JSON, through the tool call, the component and the
/// redacting wrapper, and back through the protocol's validation (unknown fields and
/// shapes are refused) into native items. Beside the items it returns the time each phase
/// of that path took, so the row's one number can be read against its parts.
async fn transfer(
    tool: &Arc<dyn Tool>,
    history: &[Item],
    serialized: usize,
) -> (Vec<Item>, Timings) {
    let (request, serialize_ms) = timed(async { serialize(history, serialized) }).await;
    let (outcome, call_ms) = timed(tool.execute(&request, context())).await;
    assert_eq!(
        outcome.status,
        ToolStatus::Ok,
        "{}",
        outcome.content.chars().take(200).collect::<String>()
    );
    let (back, validate_ms) = timed(async { validate(&outcome.content) }).await;
    (
        back,
        Timings {
            serialize: serialize_ms,
            call: call_ms,
            validate: validate_ms,
        },
    )
}

/// The call the fixture reads: its freeform input carries the mode and the payload in one
/// text, so the host's serialization of the history is written straight into that input.
/// The payload is therefore written once, where p1's own path would copy it once, and the
/// buffer is sized from the history (the same every sample) so growing it never doubles as
/// a second serialization. The call carries the ids the crate's own `call` gives the
/// fixture; only the input differs, because that one is built here.
fn serialize(history: &[Item], serialized: usize) -> ToolCall {
    let mut raw = Vec::with_capacity("echo:".len() + serialized);
    raw.extend_from_slice(b"echo:");
    let wire: Vec<WireItem> = history.iter().cloned().map(WireItem::from).collect();
    serde_json::to_writer(&mut raw, &wire).expect("history serializes");
    ToolCall {
        call_id: "c1".to_owned(),
        name: "fixture".to_owned(),
        input: ToolInput::Text(String::from_utf8(raw).expect("the wire text is UTF-8")),
    }
}

/// The protocol's validation of an answer, as the host does it: the text parses as history
/// items (unknown fields and shapes are refused) and becomes native items.
fn validate(content: &str) -> Vec<Item> {
    let wire: Vec<WireItem> =
        serde_json::from_str(content).expect("the history came back as protocol items");
    wire.into_iter().map(Item::from).collect()
}

fn wire_text(history: &[Item]) -> String {
    let wire: Vec<WireItem> = history.iter().cloned().map(WireItem::from).collect();
    serde_json::to_string(&wire).expect("history serializes")
}

/// What one transfer spent in each phase of the path, in milliseconds.
struct Timings {
    /// The host's serialization: native items to protocol wire items to JSON.
    serialize: f64,
    /// The component call: the executor's queue, the host's own call serialization, the
    /// copy into the guest, the guest's echo and the copy out.
    call: f64,
    /// The host's parse and protocol validation of the answer.
    validate: f64,
}

/// The [`Timings`] of every sample of one row.
#[derive(Default)]
struct Phases {
    serialize: Vec<f64>,
    call: Vec<f64>,
    validate: Vec<f64>,
}

impl Phases {
    fn record(&mut self, timings: Timings) {
        self.serialize.push(timings.serialize);
        self.call.push(timings.call);
        self.validate.push(timings.validate);
    }

    /// The p95 of each phase, in the order the path runs them.
    fn detail(&self) -> String {
        let phase =
            |name: &str, samples: &[f64]| format!("{name} {:.3} ms", percentile(samples, 95));
        format!(
            "{}, {}, {}",
            phase("host serialize", &self.serialize),
            phase("component call", &self.call),
            phase("host parse+validate", &self.validate)
        )
    }
}

/// A history shaped like an agent's: user turns, assistant text with a tool call, and
/// multi-line tool results, until its serialized form reaches `bytes`.
fn synthetic_history(bytes: usize) -> Vec<Item> {
    let origin = Origin {
        route: "fake/route".to_owned(),
        model: "fake-model".to_owned(),
    };
    let mut items = Vec::new();
    let mut size = 0;
    let mut turn = 0;
    while size < bytes {
        let call_id = format!("call-{turn}");
        let turn_items = [
            Item::User {
                text: synthetic_text(512, turn),
            },
            Item::Assistant(AssistantItem {
                origin: origin.clone(),
                blocks: vec![
                    AssistantBlock::Text {
                        text: synthetic_text(1024, turn + 1),
                    },
                    AssistantBlock::ToolCall(ToolCall {
                        call_id: call_id.clone(),
                        name: "read".to_owned(),
                        input: ToolInput::Json(
                            json!({"path": format!("src/module_{turn}.rs")}).to_string(),
                        ),
                    }),
                ],
            }),
            Item::ToolResult(ToolResultItem {
                call_id,
                name: "read".to_owned(),
                status: ToolStatus::Ok,
                content: synthetic_lines(4096, turn + 2),
            }),
        ];
        for item in turn_items {
            size += wire_text(std::slice::from_ref(&item)).len();
            items.push(item);
        }
        turn += 1;
    }
    items
}

/// `len` bytes of plain words, no newline, starting at word `seed`: text no redaction
/// pattern matches, so the wrapper scans it all and changes nothing.
fn synthetic_text(len: usize, seed: usize) -> String {
    const WORDS: [&str; 12] = [
        "module", "history", "agent", "boundary", "context", "result", "stream", "policy",
        "provider", "journal", "request", "item",
    ];
    let mut text = String::with_capacity(len + 16);
    let mut index = seed;
    while text.len() < len {
        text.push_str(WORDS[index % WORDS.len()]);
        text.push(' ');
        index += 1;
    }
    text.truncate(len);
    // The fixture trims the mode line, so a trailing space would not come back.
    if text.ends_with(' ') {
        text.pop();
        text.push('.');
    }
    text
}

/// As [`synthetic_text`], broken into lines like a file's content.
fn synthetic_lines(len: usize, seed: usize) -> String {
    let mut text = synthetic_text(len, seed);
    let bytes: Vec<usize> = (72..text.len()).step_by(72).collect();
    for at in bytes {
        text.replace_range(at..=at, "\n");
    }
    text
}

// ---- idle instances -------------------------------------------------------------------

/// A tool whose one call waits, idle, in `process.next` on its live instance.
struct Idle {
    _tool: Arc<dyn Tool>,
    _processes: FakeProcesses,
    process: p1_module_tests::SpawnedProcess,
    running: tokio::task::JoinHandle<ToolOutcome>,
}

async fn park_idle(module: &LoadedModule) -> Idle {
    let (tool, mut processes) = module_tool(module);
    let running = {
        let tool = tool.clone();
        tokio::spawn(async move { tool.execute(&call("stream:idle"), context()).await })
    };
    let process = processes.next_spawn().await;
    processes.wait_for(&ProcessRecord::NextWaiting).await;
    Idle {
        _tool: tool,
        _processes: processes,
        process,
        running,
    }
}

impl Idle {
    /// Lets the waiting call end, and checks it ended as the fixture's stream mode does.
    async fn finish(self) {
        self.process
            .events
            .send(ProcessEvent::Exited(ExitStatus::Code(0)))
            .expect("the idle call still waits");
        let outcome = self.running.await.expect("the idle call's task");
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    }
}

// ---- the workload ---------------------------------------------------------------------

/// What the process holds after one round of the workload.
struct Sample {
    /// `VmRSS` after the round.
    rss_kib: i64,
    /// `VmHWM` after the round: the peak since the measured rounds began.
    peak_kib: i64,
    fds: i64,
    threads: i64,
}

/// Runs `warm_up` unread rounds and then `measured` rounds of the workload over `tools`,
/// one agent per tool, and returns the reading after each measured round.
async fn rounds(
    tools: &[Arc<dyn Tool>],
    history: &Arc<Vec<Item>>,
    warm_up: usize,
    measured: usize,
) -> Vec<Sample> {
    let payload = Arc::new(synthetic_text(NOOP_PAYLOAD, 0));
    // The history is the same in every round, so the request buffer is sized once.
    let serialized = wire_text(history).len();
    let mut samples = Vec::with_capacity(measured);
    for round in 0..warm_up + measured {
        if round == warm_up {
            reset_peak();
        }
        let agents: Vec<_> = tools
            .iter()
            .map(|tool| {
                let tool = tool.clone();
                let history = history.clone();
                let payload = payload.clone();
                tokio::spawn(async move {
                    let echo = call(&format!("echo:{payload}"));
                    expect_echo(&tool.execute(&echo, context()).await, &payload);
                    let (back, _) = transfer(&tool, &history, serialized).await;
                    assert_eq!(back.len(), history.len(), "items were lost");
                })
            })
            .collect();
        for agent in agents {
            agent.await.expect("an agent's round");
        }
        if round >= warm_up {
            let memory = Memory::read();
            samples.push(Sample {
                rss_kib: memory.rss_kib,
                peak_kib: memory.peak_kib,
                fds: open_fds(),
                threads: memory.threads,
            });
        }
    }
    samples
}

// ---- readings -------------------------------------------------------------------------

/// The process's memory as the kernel accounts it.
struct Memory {
    /// `VmRSS` of `/proc/self/status`.
    rss_kib: i64,
    /// `VmHWM` of `/proc/self/status`: the peak RSS since the last [`reset_peak`].
    peak_kib: i64,
    /// `Threads` of `/proc/self/status`.
    threads: i64,
    /// Memory only this process holds: `Private_Clean + Private_Dirty + Swap` of
    /// `/proc/self/smaps_rollup`.
    private_kib: i64,
}

impl Memory {
    fn read() -> Self {
        let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
        let rollup =
            std::fs::read_to_string("/proc/self/smaps_rollup").expect("/proc/self/smaps_rollup");
        Self {
            rss_kib: field(&status, "VmRSS"),
            peak_kib: field(&status, "VmHWM"),
            threads: field(&status, "Threads"),
            private_kib: field(&rollup, "Private_Clean")
                + field(&rollup, "Private_Dirty")
                + field(&rollup, "Swap"),
        }
    }
}

/// Starts a new peak RSS measurement: `5` to `clear_refs` resets `VmHWM` to the current RSS.
fn reset_peak() {
    std::fs::write("/proc/self/clear_refs", "5").expect("reset the peak RSS");
}

/// The number of `name:` in a `/proc` key-value text.
fn field(text: &str, name: &str) -> i64 {
    text.lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name).then(|| value.split_whitespace().next()?.parse().ok())?
        })
        .unwrap_or_else(|| panic!("no {name} in the /proc reading"))
}

fn open_fds() -> i64 {
    let count = std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd")
        .count();
    i64::try_from(count).expect("a descriptor count fits")
}

fn kib_to_mib(kib: i64) -> f64 {
    kib as f64 / 1024.0
}

fn max_of(values: impl Iterator<Item = i64>) -> i64 {
    values.max().expect("at least one reading")
}

// ---- the CLI --------------------------------------------------------------------------

/// `$P1_BIN`, else the `p1` of this test binary's own profile.
fn p1_binary() -> PathBuf {
    if let Some(bin) = std::env::var_os("P1_BIN")
        && !bin.is_empty()
    {
        let path = PathBuf::from(bin);
        assert!(
            path.is_file(),
            "P1_BIN is set but {} is not a file",
            path.display()
        );
        return path;
    }
    let exe = std::env::current_exe().expect("the test binary's path");
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>");
    let p1 = profile.join("p1");
    assert!(
        p1.is_file(),
        "no p1 binary at {}: run `cargo build --locked --release -p p1-host --bin p1` \
         (scripts/bench-modules.sh --suite acceptance does)",
        p1.display()
    );
    p1
}

/// One start of the binary, with a temporary HOME and nothing else of this environment.
fn run_version(p1: &Path, home: &Path) {
    let status = Command::new(p1)
        .arg("--version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", p1.display()));
    assert!(
        status.success(),
        "{} --version exited {status}",
        p1.display()
    );
}

// ---- statistics and output ------------------------------------------------------------

async fn timed<T>(body: impl Future<Output = T>) -> (T, f64) {
    let started = Instant::now();
    let value = body.await;
    (value, millis(started.elapsed()))
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The nearest-rank percentile `p` of `samples`: the smallest sample at or above which
/// `p` percent of the samples lie, so it is always a sample that was measured.
fn percentile(samples: &[f64], p: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn measurement(statistic: &str, value: f64, unit: &str) -> Value {
    json!({
        "statistic": statistic,
        "value": (value * 1000.0).round() / 1000.0,
        "unit": unit,
    })
}

/// The case's one stdout line, which the script parses.
fn emit(row: &str, measurements: Vec<Value>, samples: usize, detail: String) {
    println!(
        "{}",
        json!({
            "acceptance-row": row,
            "measurements": measurements,
            "samples": samples,
            "detail": detail,
        })
    );
}
