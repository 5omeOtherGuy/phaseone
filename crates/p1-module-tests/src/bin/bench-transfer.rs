//! The transfer split bench (S1.11): where PLAN §10's `history-32m` "component call" phase
//! spends its time, part by part, printed as facts like [`bench-baseline`](bench-baseline.rs).
//!
//! The acceptance row times `tool.execute` on the fixture tool as one phase: the redacting
//! wrapper, `WasmTool`'s own call serialization, the executor, the canonical-ABI copy into
//! the guest, the guest's echo, the copy out and the host's read of the outcome. This bench
//! times the parts it can take apart from outside:
//!
//! - `native`: an echo in native code returning a copy of the payload, the floor;
//! - `redacting`: the same native echo behind `p1_redact::RedactingTool`, so the wrapper's
//!   cost on a large clean outcome is `redacting - native`;
//! - `host_escape` and `host_unescape`: the host's two JSON passes over the text (the call's
//!   `raw` escaped as a string literal, the outcome's `content` read back) as `serde_json`
//!   does them, which is how `WasmTool` did them before S1.11: the reference its own
//!   writer and compact reader are measured against inside `module`;
//! - `module`: the fixture's echo through `wasm_tool`, the whole phase, at 1, 8 and 32 MiB;
//!   the guest and the copies are what is left of it after the rows above.
//!
//! Each row prints its p95, its median and the process CPU time per call: on a shared box
//! the wall time also waits for other work, the CPU time much less so.
//!
//! Nothing here asserts a bound and the gate never runs it; every measured call is checked
//! to have produced the payload, so a broken path fails the bench instead of timing it.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use p1_contracts::serde_json::{self, json};
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolOutcome, ToolStatus,
};
use p1_module_runtime::{ExecutionLimits, Services, wasm_tool};
use p1_module_tests::{FIXTURE_NAME, Release, call, fake_processes};
use p1_redact::{MaskCounter, redacted};

const MIB: usize = 1024 * 1024;

/// The payload sizes of the module row, and the samples each takes: fewer for the larger
/// ones, each of which is a large transfer.
const SIZES: [(usize, usize); 3] = [(MIB, 100), (8 * MIB, 40), (32 * MIB, 20)];

/// Calls made before any sample is kept: the first calls pay for lazy initialisation.
const WARM_UP: usize = 3;

fn main() {
    // The flavour the acceptance suite's history rows run on, so the executor task and the
    // caller share the same two workers there and here.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a Tokio runtime");
    runtime.block_on(bench());
}

async fn bench() {
    println!("suite: transfer");
    println!("fixture: {FIXTURE_NAME}");
    println!("tokio_runtime: multi_thread, 2 workers");
    println!("statistic: p95 and median per part, milliseconds");

    let release = Release::with_fixture();
    let loader = release.loader();
    let module = loader.load(FIXTURE_NAME).expect("load the fixture");
    let counter = Arc::new(MaskCounter::new());
    let module_tool = wasm_tool(
        &module,
        Services {
            process: Some(fake_processes().0),
            summary: None,
        },
        ExecutionLimits::default(),
        &counter,
    )
    .expect("the fixture is a tool");

    for (size, samples) in SIZES {
        let payload = history_text(size);
        let echo = call(&format!("echo:{payload}"));
        let label = format!("{}m", size / MIB);

        let native: Arc<dyn Tool> = Arc::new(NativeEcho::new(&payload));
        report(
            &format!("native_{label}"),
            samples,
            || native.execute(&echo, context()),
            &payload,
        )
        .await;

        let wrapped = redacted(native.clone(), &counter);
        report(
            &format!("redacting_{label}"),
            samples,
            || wrapped.execute(&echo, context()),
            &payload,
        )
        .await;

        let mut serialize = Vec::with_capacity(samples);
        let mut parse = Vec::with_capacity(samples);
        let mut cpu = (Duration::ZERO, Duration::ZERO);
        for _ in 0..samples {
            let (started, before) = (Instant::now(), process_cpu());
            let literal = serde_json::to_string(&payload).expect("a string serializes");
            let (middle, between) = (Instant::now(), process_cpu());
            let back: String = serde_json::from_str(&literal).expect("and parses back");
            let after = process_cpu();
            serialize.push(middle - started);
            parse.push(middle.elapsed());
            cpu.0 += between.saturating_sub(before);
            cpu.1 += after.saturating_sub(between);
            assert_eq!(back.len(), payload.len(), "host-json changed the payload");
        }
        print_samples(&format!("host_escape_{label}"), &mut serialize, cpu.0);
        print_samples(&format!("host_unescape_{label}"), &mut parse, cpu.1);

        report(
            &format!("module_{label}"),
            samples,
            || module_tool.execute(&echo, context()),
            &payload,
        )
        .await;
        assert_eq!(counter.take(), 0, "a clean history was masked");
    }
}

/// Times `samples` calls of `execute` after the warm-up, checking each answer.
async fn report<'a, F, Fut>(name: &str, samples: usize, execute: F, payload: &str)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ToolOutcome> + 'a,
{
    for _ in 0..WARM_UP {
        check(&execute().await, payload, name);
    }
    let mut taken = Vec::with_capacity(samples);
    let cpu = process_cpu();
    for _ in 0..samples {
        let started = Instant::now();
        let outcome = execute().await;
        taken.push(started.elapsed());
        check(&outcome, payload, name);
    }
    // The checks are inside this reading; they compare two strings, far below a call.
    print_samples(name, &mut taken, process_cpu().saturating_sub(cpu));
}

/// Prints the p95 and median of `samples` and the mean process CPU time of one sample:
/// on a shared box the wall time waits for other work too, the CPU time much less so.
fn print_samples(name: &str, samples: &mut [Duration], cpu: Duration) {
    samples.sort_unstable();
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    let median = samples[samples.len() / 2];
    let count = u32::try_from(samples.len()).expect("a sample count fits u32");
    println!(
        "{name}: p95 {} median {} cpu/call {} ({} samples)",
        millis(p95),
        millis(median),
        millis(cpu / count),
        samples.len()
    );
}

/// The CPU time of this process so far, user and system, every thread: fields 14 and 15 of
/// `/proc/self/stat`, in the kernel's USER_HZ ticks (100 per second on Linux).
fn process_cpu() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    // The command name (field 2) may hold spaces; the fields after its `)` do not.
    let fields: Vec<&str> = stat[stat.rfind(')').expect("a stat line") + 1..]
        .split_whitespace()
        .collect();
    let ticks = |index: usize| fields[index].parse::<u64>().expect("a tick count");
    // `fields[0]` is field 3 (state), so fields 14 and 15 are at 11 and 12.
    Duration::from_millis((ticks(11) + ticks(12)) * 10)
}

fn millis(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

fn check(outcome: &ToolOutcome, payload: &str, what: &str) {
    assert_eq!(outcome.status, ToolStatus::Ok, "{what}");
    assert!(
        outcome.content == payload,
        "{what}: the echo changed the payload"
    );
}

fn context() -> ToolContext {
    ToolContext {
        cancel: CancellationToken::new(),
    }
}

/// JSON text of at least `bytes` bytes shaped like a serialized history: one line, many
/// short string values, quotes to escape and multi-line tool results, and nothing any
/// redaction pattern matches, as the acceptance row's synthetic history.
fn history_text(bytes: usize) -> String {
    const WORDS: [&str; 12] = [
        "module", "history", "agent", "boundary", "context", "result", "stream", "policy",
        "provider", "journal", "request", "item",
    ];
    let words = |len: usize, seed: usize, line: Option<usize>| {
        let mut text = String::with_capacity(len + 16);
        let mut index = seed;
        while text.len() < len {
            text.push_str(WORDS[index % WORDS.len()]);
            let at_break = line.is_some_and(|line| text.len() / line != (text.len() + 1) / line);
            text.push(if at_break { '\n' } else { ' ' });
            index += 1;
        }
        text.truncate(len);
        text.push('.');
        text
    };
    let mut items = Vec::new();
    let mut size = 0;
    let mut turn = 0;
    while size < bytes {
        let turn_items = [
            json!({"item": "user", "text": words(512, turn, None)}),
            json!({"item": "assistant", "origin": {"route": "fake/route", "model": "fake-model"},
                "blocks": [{"block": "text", "text": words(1024, turn + 1, None)},
                    {"block": "tool_call", "call_id": format!("call-{turn}"), "name": "read",
                     "input": {"kind": "json",
                        "raw": json!({"path": format!("src/module_{turn}.rs")}).to_string()}}]}),
            json!({"item": "tool_result", "call_id": format!("call-{turn}"), "name": "read",
                "status": "ok", "content": words(4096, turn + 2, Some(72))}),
        ];
        for item in turn_items {
            size += item.to_string().len() + 1;
            items.push(item);
        }
        turn += 1;
    }
    serde_json::to_string(&items).expect("the history serializes")
}

/// The fixture's echo in native code, answering with a copy of the payload it was built
/// with: the copy the module path also makes, and nothing else.
struct NativeEcho {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    payload: String,
}

impl NativeEcho {
    fn new(payload: &str) -> Self {
        Self {
            declaration: ToolDeclaration {
                name: "fixture".to_owned(),
                description: "native echo".to_owned(),
                kind: DeclarationKind::Freeform { grammar: None },
            },
            identity: ToolIdentity {
                implementation: "native/echo".to_owned(),
                variant: "default".to_owned(),
            },
            payload: payload.to_owned(),
        }
    }
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
        _call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { ToolOutcome::ok(self.payload.clone()) })
    }
}
