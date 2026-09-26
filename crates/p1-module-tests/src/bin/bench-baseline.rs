//! The baseline cost bench (S0.8): what the fixture costs the runtime at each moment a host
//! pays a cost, measured with `std::time::Instant` and printed as facts for
//! [`scripts/bench-modules.sh`](../../../../scripts/bench-modules.sh).
//!
//! Nothing here asserts a bound: the readings are wall-clock facts about one box and one
//! commit, and the programme lead compares them. Every measured call is checked to have
//! produced the fixture's answer, so a broken path fails the bench instead of reporting a
//! meaningless time.
//!
//! Cold, the order a host pays it in: the engine, the loader over the harness's release, the
//! first `load` (read + SHA-256 verify + compile), the tool over that module, and the first
//! `execute`. Warm: `WARM_CALLS` calls of `execute` on the loaded tool, and `WARM_CALLS`
//! synchronous `describe` calls on the restricted path; each is reported as its median and
//! its maximum.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use p1_contracts::{CallDescription, CancellationToken, ToolContext, ToolOutcome, ToolStatus};
use p1_module_runtime::{ExecutionLimits, Services, wasm_tool};
use p1_module_tests::{FIXTURE_NAME, Release, call, fake_processes};

/// How many warm samples each warm measurement takes. Printed with the readings, because
/// they describe exactly this many calls and mean nothing without the count.
const WARM_CALLS: usize = 50;

fn main() {
    // One current-thread runtime: a reading here is then the module runtime's own cost, not a
    // worker pool's startup, and it is the flavour a host that calls a module inline uses.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a Tokio runtime");
    runtime.block_on(bench());
}

async fn bench() {
    println!("suite: baseline");
    println!("fixture: {FIXTURE_NAME}");
    println!("tokio_runtime: current_thread");
    println!("warm_calls: {WARM_CALLS} calls");

    // The engine a loader builds for itself, so that its compile-time configuration has a
    // reading of its own; the loader below builds a second one, as a host's first load does.
    let (_engine, cold_engine) = time(|| p1_module_runtime::engine().expect("the wasmtime engine"));
    println!("cold_engine_creation: {}", millis(cold_engine));

    // Outside every reading: the harness writes the fixture into a temporary release
    // directory and its entry into a release manifest. The loader's own read of that
    // manifest is inside `cold_loader_creation`.
    let release = Release::with_fixture();

    let (loader, cold_loader) = time(|| release.loader());
    println!("cold_loader_creation: {}", millis(cold_loader));

    let (module, cold_load) = time(|| loader.load(FIXTURE_NAME).expect("load the fixture"));
    println!("cold_load: {}", millis(cold_load));

    // The tool over the loaded module: pre-linked, pre-instantiated, its declaration read
    // through the restricted path, and its executor task started.
    let (tool, cold_tool) = time(|| {
        // The mask counter `wasm_tool` takes, built through its `Default` impl with the type
        // taken from that signature: `p1-redact` is a dev-dependency of this crate, which a
        // bin target cannot use, and this bench needs the counter, not the crate's name.
        let counter: Arc<_> = Arc::new(Default::default());
        wasm_tool(
            &module,
            Services {
                process: Some(fake_processes().0),
            },
            ExecutionLimits::default(),
            &counter,
        )
        .expect("the fixture is a tool")
    });
    println!("cold_tool_construction: {}", millis(cold_tool));

    let echo = call("echo:hi");
    let (first, cold_execute) = time_async(tool.execute(&echo, context())).await;
    check(&first, "the first execute");
    println!("cold_first_execute: {}", millis(cold_execute));

    let mut executes = Vec::with_capacity(WARM_CALLS);
    for _ in 0..WARM_CALLS {
        let (outcome, took) = time_async(tool.execute(&echo, context())).await;
        check(&outcome, "a warm execute");
        executes.push(took);
    }
    let (median, max) = median_and_max(&mut executes);
    println!("warm_execute_median: {}", millis(median));
    println!("warm_execute_max: {}", millis(max));

    // `describe` is synchronous on the host's own thread, through the restricted path: the
    // module's instance there is built once, so these readings are the restricted call alone.
    let mut describes = Vec::with_capacity(WARM_CALLS);
    for _ in 0..WARM_CALLS {
        let (described, took) = time(|| tool.describe(&echo));
        check_description(&described);
        describes.push(took);
    }
    let (median, max) = median_and_max(&mut describes);
    println!("warm_describe_median: {}", millis(median));
    println!("warm_describe_max: {}", millis(max));
}

/// A call context with a cancellation that never fires: this bench measures a call that runs
/// to its end.
fn context() -> ToolContext {
    ToolContext {
        cancel: CancellationToken::new(),
    }
}

/// `body`'s value and the wall time it took.
fn time<T>(body: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = body();
    (value, started.elapsed())
}

/// The same for a future, awaited: `execute` hands back a boxed future that runs its call on
/// the executor.
async fn time_async<T>(body: impl Future<Output = T>) -> (T, Duration) {
    let started = Instant::now();
    let value = body.await;
    (value, started.elapsed())
}

/// The median and the maximum of `samples`, sorted in place. The median of an even count is
/// the mean of its two middle samples.
fn median_and_max(samples: &mut [Duration]) -> (Duration, Duration) {
    samples.sort_unstable();
    let max = *samples.last().expect("at least one sample");
    let middle = samples.len() / 2;
    let median = if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2
    } else {
        samples[middle]
    };
    (median, max)
}

/// A duration in milliseconds with three decimals: a warm call is far below one millisecond,
/// which a whole-millisecond reading would round to zero.
fn millis(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1000.0)
}

/// Fails the bench unless the call produced the fixture's answer: a time measured on a path
/// that did not work is not a fact about the runtime.
fn check(outcome: &ToolOutcome, what: &str) {
    assert_eq!(
        outcome.status,
        ToolStatus::Ok,
        "{what}: {}",
        outcome.content
    );
    assert_eq!(outcome.content, "hi", "{what}");
}

/// The same for the restricted path: the fixture describes `echo:hi` as its own call.
fn check_description(described: &CallDescription) {
    assert_eq!(described.verb, "call", "described verb");
    assert_eq!(described.target.as_deref(), Some("echo:hi"));
    assert!(!described.destructive);
}
