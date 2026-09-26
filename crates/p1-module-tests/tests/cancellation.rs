//! Cancellation and the streaming-resource contract (S0.6, freeze items 4 and 10): epoch
//! deadline, fuel, cooperative `control.cancelled()`, the cancellation interrupt of a CPU
//! loop, a blocked `process.spawn` and a blocked `process.running.next` woken by a
//! cancellation, the caller dropping a call while it waits, the guest dropping its resource,
//! `next` after the terminal event, and the rule that a trap never undoes a native effect.
//!
//! Every case runs on a current-thread and a multi-thread Tokio runtime under the deadlock
//! guard. Epochs never advance on their own here (`Loader::with_manual_epochs`) and every
//! wait is on an explicit signal from the fake process service, never on a sleep. Where a
//! case must act on a guest that is already running a CPU loop, it uses the fixture's
//! `announce:<mode>` prefix: the guest spawns the command `announce` before the loop, and
//! the fake's record of that spawn is the signal. Where it must act on a guest blocked
//! inside `process.spawn`, it uses the gated fake ([`gated_processes`]), whose `spawn`
//! stays pending until the case settles it.

use std::sync::Arc;

use p1_contracts::{CancellationToken, Tool, ToolContext, ToolStatus};
use p1_module_runtime::loader::EPOCH_TICK;
use p1_module_runtime::{
    ExecutionLimits, ExitStatus, ManualEpochs, ProcessEvent, ProcessService, Services, wasm_tool,
};
use p1_module_tests::{
    FIXTURE_NAME, FakeProcesses, KILLED_EXIT, ProcessRecord, Release, call, fake_processes,
    gated_processes,
};
use p1_redact::MaskCounter;

/// Generates the two flavours of case `$case` (an `async fn` below) as
/// `$case::current_thread` and `$case::multi_thread`.
macro_rules! on_both_flavours {
    ($($case:ident),* $(,)?) => {$(
        mod $case {
            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() {
                p1_module_tests::within_deadline(
                    concat!(stringify!($case), " (current_thread)"),
                    super::$case(),
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                p1_module_tests::within_deadline(
                    concat!(stringify!($case), " (multi_thread)"),
                    super::$case(),
                )
                .await;
            }
        }
    )*};
}

on_both_flavours!(
    an_epoch_deadline_stops_a_cpu_loop,
    a_small_fuel_budget_stops_a_cpu_loop,
    a_cooperative_guest_returns_cancelled,
    cancellation_interrupts_a_cpu_loop,
    a_cancellation_during_spawn_ends_the_call_cancelled,
    a_refused_cancelled_spawn_ends_the_call_cancelled,
    cancellation_wakes_a_blocked_next,
    dropping_the_call_while_next_is_blocked_kills_the_process,
    a_guest_drop_kills_the_process,
    next_after_the_terminal_event_traps_and_keeps_the_effect,
    a_trapped_or_cancelled_call_poisons_nothing,
);

/// The deadline of the deadline case, in epoch ticks.
const DEADLINE_TICKS: u32 = 5;

/// A fuel budget that instantiates the fixture and answers `echo:`, but ends `spin` soon.
const SMALL_FUEL: u64 = 20_000_000;

/// The fixture tool over the fake process service, with epochs the case drives.
struct Case {
    tool: Arc<dyn Tool>,
    processes: FakeProcesses,
    epochs: ManualEpochs,
}

fn case(limits: ExecutionLimits) -> Case {
    let (process, processes) = fake_processes();
    let (tool, epochs) = tool_over(process, limits);
    Case {
        tool,
        processes,
        epochs,
    }
}

/// The fixture tool over `process`, with epochs only [`ManualEpochs::advance`] moves.
fn tool_over(
    process: Arc<dyn ProcessService>,
    limits: ExecutionLimits,
) -> (Arc<dyn Tool>, ManualEpochs) {
    let release = Release::with_fixture();
    let (loader, epochs) = release.loader_with_manual_epochs();
    let module = loader.load(FIXTURE_NAME).expect("load the fixture");
    let tool = wasm_tool(
        &module,
        Services {
            process: Some(process),
        },
        limits,
        &Arc::new(MaskCounter::new()),
    )
    .expect("the fixture is a tool");
    (tool, epochs)
}

fn context(cancel: &CancellationToken) -> ToolContext {
    ToolContext {
        cancel: cancel.clone(),
    }
}

/// Starts `raw` on its own task, as the core runs a tool call.
fn start(
    tool: &Arc<dyn Tool>,
    raw: &str,
    cancel: &CancellationToken,
) -> tokio::task::JoinHandle<p1_contracts::ToolOutcome> {
    let tool = tool.clone();
    let request = call(raw);
    let context = context(cancel);
    tokio::spawn(async move { tool.execute(&request, context).await })
}

/// The same tool still answers a plain call.
async fn assert_usable(tool: &Arc<dyn Tool>, after: &str) {
    let text = format!("after {after}");
    let outcome = tool
        .execute(
            &call(&format!("echo:{text}")),
            context(&CancellationToken::new()),
        )
        .await;
    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.content, text);
}

/// Waits until the announcing guest has spawned `announce`, so it is inside its loop.
async fn wait_for_announce(processes: &mut FakeProcesses) {
    let spawned = processes.next_spawn().await;
    assert_eq!(spawned.command.script, "announce");
}

async fn an_epoch_deadline_stops_a_cpu_loop() {
    let Case {
        tool,
        mut processes,
        epochs,
    } = case(ExecutionLimits {
        deadline: EPOCH_TICK * DEADLINE_TICKS,
        ..ExecutionLimits::default()
    });
    let cancel = CancellationToken::new();
    let running = start(&tool, "announce:spin", &cancel);
    wait_for_announce(&mut processes).await;

    // One tick short of the deadline changes nothing observable; past it, the call ends.
    epochs.advance(u64::from(DEADLINE_TICKS) - 1);
    assert!(!running.is_finished(), "ended before its deadline");
    epochs.advance(1);
    let outcome = running.await.expect("the executing task");
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("exceeded its time limit"),
        "not DeadlineExceeded: {}",
        outcome.content
    );
    // The resource the guest still held was dropped with the call, killing the process.
    let records = processes.wait_for(&ProcessRecord::Dropped).await;
    assert!(records.contains(&ProcessRecord::Killed), "{records:?}");
    assert_usable(&tool, "the deadline").await;
}

async fn a_small_fuel_budget_stops_a_cpu_loop() {
    let Case { tool, .. } = case(ExecutionLimits {
        fuel: SMALL_FUEL,
        ..ExecutionLimits::default()
    });
    let outcome = tool
        .execute(&call("spin"), context(&CancellationToken::new()))
        .await;
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("exhausted its compute budget"),
        "not FuelExhausted: {}",
        outcome.content
    );
    // Each call starts with the full budget again.
    assert_usable(&tool, "fuel exhaustion").await;
}

async fn a_cooperative_guest_returns_cancelled() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "announce:cooperative", &cancel);
    wait_for_announce(&mut processes).await;

    cancel.cancel();
    let outcome = running.await.expect("the executing task");
    assert_eq!(outcome.status, ToolStatus::Cancelled, "{}", outcome.content);
    assert_eq!(outcome.content, "");
    let records = processes.wait_for(&ProcessRecord::Dropped).await;
    assert!(records.contains(&ProcessRecord::Killed), "{records:?}");
    assert_usable(&tool, "a cooperative cancellation").await;
}

async fn cancellation_interrupts_a_cpu_loop() {
    // No epoch is ever advanced here, so the call cannot end by its deadline: only the
    // cancellation interrupt ends a loop that never calls an import.
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "announce:spin", &cancel);
    wait_for_announce(&mut processes).await;

    cancel.cancel();
    let outcome = running.await.expect("the executing task");
    assert_eq!(
        outcome.status,
        ToolStatus::Cancelled,
        "not Cancelled: {}",
        outcome.content
    );
    assert_eq!(outcome.content, "");
    let records = processes.wait_for(&ProcessRecord::Dropped).await;
    assert!(records.contains(&ProcessRecord::Killed), "{records:?}");
    assert_usable(&tool, "an interrupted loop").await;
}

async fn a_cancellation_during_spawn_ends_the_call_cancelled() {
    // The guest is blocked inside `process.spawn` when the cancellation fires. The service
    // settles the start it was asked for, and the guest reads the cancellation in the one
    // place `process.wit` has for it — the resource's `exited(cancelled)`, which the
    // fixture's `stream` mode answers with its `cancelled` status. It is never a `spawn`
    // error, which no guest could tell from a command that could not start.
    let (process, mut processes, gate) = gated_processes();
    let (tool, _epochs) = tool_over(process, ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "stream:make it", &cancel);
    processes.wait_for(&ProcessRecord::SpawnWaiting).await;

    cancel.cancel();
    gate.start();
    let outcome = running.await.expect("the executing task");
    assert_eq!(outcome.status, ToolStatus::Cancelled, "{}", outcome.content);
    assert_eq!(outcome.content, "");
    let records = processes.wait_for(&ProcessRecord::Dropped).await.to_vec();
    assert!(
        records.contains(&ProcessRecord::Spawned("make it".to_owned())),
        "{records:?}"
    );
    // The host killed the group of the call that was cancelled, as `process.wit` says.
    assert!(records.contains(&ProcessRecord::Killed), "{records:?}");
    assert_usable(&tool, "a cancellation during spawn").await;
}

async fn a_refused_cancelled_spawn_ends_the_call_cancelled() {
    // A service may notice the cancellation while it starts and refuse the command. That is
    // not a failure the guest may act on either: `process.wit` gives `spawn` no cancellation
    // return, so the host hands the guest a resource that reports the cancelled ending at
    // once, and nothing was started to kill or drop.
    let (process, mut processes, gate) = gated_processes();
    let (tool, _epochs) = tool_over(process, ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "stream:make it", &cancel);
    processes.wait_for(&ProcessRecord::SpawnWaiting).await;

    cancel.cancel();
    gate.refuse();
    let outcome = running.await.expect("the executing task");
    assert_eq!(outcome.status, ToolStatus::Cancelled, "{}", outcome.content);
    assert_eq!(outcome.content, "");
    let records = processes.records().to_vec();
    assert!(
        !records.contains(&ProcessRecord::Spawned("make it".to_owned())),
        "{records:?}"
    );
    assert_usable(&tool, "a refused cancelled spawn").await;
}

async fn cancellation_wakes_a_blocked_next() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "stream:make it", &cancel);
    let spawned = processes.next_spawn().await;
    assert_eq!(spawned.command.script, "make it");
    // The first `next` is blocked on the host wait: the fake has no event for it.
    processes.wait_for(&ProcessRecord::NextWaiting).await;
    assert!(!processes.records().contains(&ProcessRecord::Killed));

    cancel.cancel();
    let outcome = running.await.expect("the executing task");
    // The guest's `stream` mode answers `cancelled` only when `next` returned
    // `exited(cancelled)`, which the host reports after killing the process group.
    assert_eq!(outcome.status, ToolStatus::Cancelled, "{}", outcome.content);
    let records = processes.wait_for(&ProcessRecord::Dropped).await.to_vec();
    let killed = records
        .iter()
        .position(|record| *record == ProcessRecord::Killed)
        .unwrap_or_else(|| panic!("no kill recorded: {records:?}"));
    let exited = records
        .iter()
        .position(|record| *record == ProcessRecord::Exited(KILLED_EXIT))
        .unwrap_or_else(|| panic!("no exit after the kill: {records:?}"));
    assert!(killed < exited, "{records:?}");
    spawned.dropped.await.expect("the process handle dropped");
    assert_usable(&tool, "a cancelled stream").await;
}

async fn dropping_the_call_while_next_is_blocked_kills_the_process() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let request = call("stream:wait forever");
    let mut execute = tool.execute(&request, context(&CancellationToken::new()));
    tokio::select! {
        outcome = &mut execute => panic!("the call ended while its process runs: {outcome:?}"),
        _ = processes.wait_for(&ProcessRecord::NextWaiting) => {}
    }

    // The caller gives up on the call while `next` waits.
    drop(execute);
    let records = processes.wait_for(&ProcessRecord::Dropped).await;
    assert!(records.contains(&ProcessRecord::Killed), "{records:?}");
    // The executor stays usable.
    assert_usable(&tool, "an abandoned call").await;
}

async fn a_guest_drop_kills_the_process() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "stream-drop:watch", &cancel);
    let spawned = processes.next_spawn().await;
    processes.wait_for(&ProcessRecord::NextWaiting).await;
    spawned
        .events
        .send(ProcessEvent::Output(b"first".to_vec()))
        .expect("send output");

    let outcome = running.await.expect("the executing task");
    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.content, "dropped");
    let records = processes.wait_for(&ProcessRecord::Dropped).await;
    assert_eq!(
        records,
        [
            ProcessRecord::Spawned("watch".to_owned()),
            ProcessRecord::NextWaiting,
            ProcessRecord::Output(b"first".to_vec()),
            ProcessRecord::Killed,
            ProcessRecord::Dropped,
        ]
    );
    assert_usable(&tool, "a guest drop").await;
}

async fn next_after_the_terminal_event_traps_and_keeps_the_effect() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "trap-after-terminal:touch made-it", &cancel);
    let spawned = processes.next_spawn().await;
    let marker = processes.markers().join("made-it");
    // The command's native effect happened before the module read any event.
    processes
        .wait_for(&ProcessRecord::Effect(marker.clone()))
        .await;
    spawned
        .events
        .send(ProcessEvent::Output(b"made".to_vec()))
        .expect("send output");
    spawned
        .events
        .send(ProcessEvent::Exited(ExitStatus::Code(0)))
        .expect("send exit");

    let outcome = running.await.expect("the executing task");
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("trapped"),
        "not a trap: {}",
        outcome.content
    );
    assert!(
        outcome.content.contains("after the stream ended"),
        "the trap is not named: {}",
        outcome.content
    );
    // A trap never undoes a native effect.
    assert!(marker.is_file(), "the marker file is gone");
    let records = processes.wait_for(&ProcessRecord::Dropped).await;
    assert!(
        records.contains(&ProcessRecord::Exited(ExitStatus::Code(0))),
        "{records:?}"
    );
    // The process had exited: nothing was left to kill.
    assert!(!records.contains(&ProcessRecord::Killed), "{records:?}");
    assert_usable(&tool, "a trap after the terminal event").await;
}

async fn a_trapped_or_cancelled_call_poisons_nothing() {
    let Case {
        tool,
        mut processes,
        epochs,
    } = case(ExecutionLimits {
        deadline: EPOCH_TICK * DEADLINE_TICKS,
        ..ExecutionLimits::default()
    });

    let trap = tool
        .execute(&call("trap"), context(&CancellationToken::new()))
        .await;
    assert_eq!(trap.status, ToolStatus::Error, "{}", trap.content);
    assert!(trap.content.contains("unreachable"), "{}", trap.content);
    assert_usable(&tool, "a guest trap").await;

    let cancel = CancellationToken::new();
    let running = start(&tool, "announce:spin", &cancel);
    wait_for_announce(&mut processes).await;
    cancel.cancel();
    let cancelled = running.await.expect("the executing task");
    assert_eq!(
        cancelled.status,
        ToolStatus::Cancelled,
        "{}",
        cancelled.content
    );
    assert_usable(&tool, "a cancelled loop").await;

    let running = start(&tool, "announce:spin", &CancellationToken::new());
    wait_for_announce(&mut processes).await;
    epochs.advance(u64::from(DEADLINE_TICKS));
    let late = running.await.expect("the executing task");
    assert_eq!(late.status, ToolStatus::Error, "{}", late.content);
    assert!(
        late.content.contains("exceeded its time limit"),
        "{}",
        late.content
    );
    // Deadlines count from each call's start: the next call has a whole one of its own.
    assert_usable(&tool, "a deadline").await;
}
