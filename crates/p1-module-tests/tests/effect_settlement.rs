//! Effect settlement at the WebAssembly boundary (S3.5): what becomes of a native effect a
//! tool module started through the `process` capability when the call that started it ends
//! by a trap, a deadline, a fuel exhaustion or a cancellation.
//!
//! Two halves of one rule are pinned here. [`cancellation.md`]'s "a trap never undoes a
//! native effect": a command that ran, or a file it wrote, stays done however the call ends.
//! And its settlement half: a resource does not outlive the export call that created it, so
//! the runtime drops the call's Store — and with it every `process.running` the guest still
//! held — *before* it answers the caller. A started process is therefore killed and reaped
//! (the fake service records `Killed`/`Dropped`) before `execute` returns, the answer is
//! never a success, and no failed call is retried automatically.
//!
//! Every case runs on a current-thread and a multi-thread Tokio runtime under the harness's
//! deadlock guard and is bounded by it. Synchronization is explicit, through the fake process
//! service's records: no case waits on a sleep. The cases build on S0's `cancellation.rs`
//! (`an_epoch_deadline_stops_a_cpu_loop` and `a_small_fuel_budget_stops_a_cpu_loop`, whose
//! `announce:spin`/`ManualEpochs` and `SMALL_FUEL` setups the deadline and fuel cases here
//! refocus on settlement before the answer, `cancellation_wakes_a_blocked_next`,
//! `next_after_the_terminal_event_traps_and_keeps_the_effect`,
//! `a_trapped_or_cancelled_call_poisons_nothing`) and do not repeat them; where a case here
//! restates one of those, it is because it also owes the settlement contract a caller relies
//! on — the effect settled *before* the answer, the answer never `Ok`, the call never retried.
//!
//! [`cancellation.md`]: ../../../docs/design/modules/cancellation.md

use std::sync::Arc;

use p1_contracts::{CancellationToken, Tool, ToolContext, ToolOutcome, ToolStatus};
use p1_module_runtime::loader::EPOCH_TICK;
use p1_module_runtime::{
    ExecutionLimits, ExitStatus, ManualEpochs, ProcessEvent, ProcessService, Services, wasm_tool,
};
use p1_module_tests::{FIXTURE_NAME, FakeProcesses, ProcessRecord, Release, call, fake_processes};
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
    a_trap_after_spawn_kills_the_process_before_the_call_returns,
    a_deadline_after_spawn_settles_the_effect,
    a_fuel_exhaustion_after_spawn_settles_the_effect,
    a_trap_never_undoes_the_effect,
    cancellation_waits_for_the_started_effect_to_settle,
    a_failed_call_is_not_retried,
    an_unestablished_effect_is_never_success_and_carries_guidance,
);

/// The deadline of the deadline cases, in epoch ticks; the same small bound S0's cancellation
/// suite uses, because the point is when the call ends, not how long it was given.
const DEADLINE_TICKS: u32 = 5;

/// A fuel budget that instantiates the fixture, spawns `announce` and then ends `spin` soon.
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
            summary: None,
            completion: None,
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
) -> tokio::task::JoinHandle<ToolOutcome> {
    let tool = tool.clone();
    let request = call(raw);
    let context = context(cancel);
    tokio::spawn(async move { tool.execute(&request, context).await })
}

/// The same tool still answers a plain call, so a failed call poisoned nothing.
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

/// Waits until the announcing guest has spawned `announce`, so it is running its mode while
/// still holding the resource the case will end with the call.
async fn wait_for_announce(processes: &mut FakeProcesses) {
    let spawned = processes.next_spawn().await;
    assert_eq!(spawned.command.script, "announce");
}

/// Whether the fake recorded the settlement of a started process: the kill or the drop of the
/// runtime's handle, whichever the service's contract allowed.
fn settled(records: &[ProcessRecord]) -> bool {
    records
        .iter()
        .any(|record| matches!(record, ProcessRecord::Killed | ProcessRecord::Dropped))
}

/// How many commands the fake started, over the records seen so far.
fn spawned_count(records: &[ProcessRecord]) -> usize {
    records
        .iter()
        .filter(|record| matches!(record, ProcessRecord::Spawned(_)))
        .count()
}

/// How many times the fake started the command `script`.
fn spawns_of(records: &[ProcessRecord], script: &str) -> usize {
    records
        .iter()
        .filter(|record| matches!(record, ProcessRecord::Spawned(name) if name.as_str() == script))
        .count()
}

/// Drives `trap-after-terminal:touch <name>` to its trap: the command writes its marker when
/// it starts, then the guest drains the resource and calls `next` once more, which the host
/// traps ([`modules/wit/process.wit`]'s "calling `next` again after `none` traps").
async fn trap_after_touch(
    tool: &Arc<dyn Tool>,
    processes: &mut FakeProcesses,
    name: &str,
) -> ToolOutcome {
    let cancel = CancellationToken::new();
    let running = start(tool, &format!("trap-after-terminal:touch {name}"), &cancel);
    let spawned = processes.next_spawn().await;
    assert_eq!(spawned.command.script, format!("touch {name}"));
    // The native effect happened when the command started, before the guest read any event.
    let marker = processes.markers().join(name);
    processes.wait_for(&ProcessRecord::Effect(marker)).await;
    // The token `touch` starts nothing that exits on its own, so the case sends the terminal
    // event the guest drains, and the guest's extra `next` reaches the trap.
    spawned
        .events
        .send(ProcessEvent::Output(b"made".to_vec()))
        .expect("send output");
    spawned
        .events
        .send(ProcessEvent::Exited(ExitStatus::Code(0)))
        .expect("send exit");
    running.await.expect("the executing task")
}

async fn a_trap_after_spawn_kills_the_process_before_the_call_returns() {
    // `announce:trap` spawns `announce`, keeps the resource without reading it and then traps.
    // The call ends before the guest ever dropped the resource, so the runtime must settle it.
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "announce:trap", &cancel);
    wait_for_announce(&mut processes).await;

    let outcome = running.await.expect("the executing task");
    // The records are the ones the fake had written when the future resolved: the executor
    // sends the answer only after it has dropped the call's Store, and the running handle
    // with it, so the settlement precedes the caller's `await`.
    let records = processes.records().to_vec();
    assert!(
        settled(&records),
        "the started process outlived the call: {records:?}"
    );
    assert_ne!(
        outcome.status,
        ToolStatus::Ok,
        "a trap after spawn answered success: {}",
        outcome.content
    );
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("check the state"),
        "no reconciliation guidance: {}",
        outcome.content
    );
    assert_eq!(
        spawned_count(&records),
        1,
        "the call was retried: {records:?}"
    );
    assert_usable(&tool, "a trap after spawn").await;
}

async fn a_deadline_after_spawn_settles_the_effect() {
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

    // The guest holds the resource and spins; past the deadline the call ends.
    epochs.advance(u64::from(DEADLINE_TICKS));
    let outcome = running.await.expect("the executing task");
    let records = processes.records().to_vec();
    assert!(
        settled(&records),
        "the started process outlived the deadline: {records:?}"
    );
    assert_ne!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("exceeded its time limit"),
        "not a deadline: {}",
        outcome.content
    );
    assert!(
        outcome.content.contains("check the state"),
        "no reconciliation guidance: {}",
        outcome.content
    );
    assert_eq!(
        spawned_count(&records),
        1,
        "the call was retried: {records:?}"
    );
    assert_usable(&tool, "a deadline after spawn").await;
}

async fn a_fuel_exhaustion_after_spawn_settles_the_effect() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits {
        fuel: SMALL_FUEL,
        ..ExecutionLimits::default()
    });
    let cancel = CancellationToken::new();
    let running = start(&tool, "announce:spin", &cancel);
    wait_for_announce(&mut processes).await;

    let outcome = running.await.expect("the executing task");
    let records = processes.records().to_vec();
    assert!(
        settled(&records),
        "the started process outlived the failed call: {records:?}"
    );
    assert_ne!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("exhausted its compute budget"),
        "not a fuel exhaustion: {}",
        outcome.content
    );
    assert!(
        outcome.content.contains("check the state"),
        "no reconciliation guidance: {}",
        outcome.content
    );
    assert_eq!(
        spawned_count(&records),
        1,
        "the call was retried: {records:?}"
    );
    assert_usable(&tool, "a fuel exhaustion after spawn").await;
}

async fn a_trap_never_undoes_the_effect() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let name = "made-it";
    let outcome = trap_after_touch(&tool, &mut processes, name).await;

    assert_ne!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("after the stream ended"),
        "not the trap after the terminal event: {}",
        outcome.content
    );
    // A trap never undoes a native effect: the command's marker file still exists.
    assert!(
        processes.markers().join(name).is_file(),
        "the marker file is gone"
    );
    let records = processes.records().to_vec();
    assert_eq!(
        spawns_of(&records, &format!("touch {name}")),
        1,
        "the call was retried: {records:?}"
    );
    // The command had already exited, so nothing was left to kill; its handle was still
    // dropped with the call, as the resource does not outlive the export that created it.
    assert!(records.contains(&ProcessRecord::Dropped), "{records:?}");
    assert!(!records.contains(&ProcessRecord::Killed), "{records:?}");
    assert_usable(&tool, "a trap after the terminal event").await;
}

async fn cancellation_waits_for_the_started_effect_to_settle() {
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let cancel = CancellationToken::new();
    let running = start(&tool, "stream:make it", &cancel);
    let spawned = processes.next_spawn().await;
    assert_eq!(spawned.command.script, "make it");
    // The guest is blocked in its first `next`: the fake has no event for it yet.
    processes.wait_for(&ProcessRecord::NextWaiting).await;

    cancel.cancel();
    let outcome = running.await.expect("the executing task");
    // The sequence as it stood the instant the future resolved. The settlement records are
    // in it because the executor emits them while it drops the call's Store, before it sends
    // the answer, so the call awaited the effect it had started.
    let records = processes.records().to_vec();
    let killed = records
        .iter()
        .position(|record| *record == ProcessRecord::Killed)
        .unwrap_or_else(|| panic!("the future completed before the kill: {records:?}"));
    let dropped = records
        .iter()
        .position(|record| *record == ProcessRecord::Dropped)
        .unwrap_or_else(|| panic!("the future completed before the drop: {records:?}"));
    assert!(killed < dropped, "{records:?}");
    assert_eq!(outcome.status, ToolStatus::Cancelled, "{}", outcome.content);
    assert_eq!(outcome.content, "");
    assert_usable(&tool, "a cancelled stream").await;
}

async fn a_failed_call_is_not_retried() {
    let Case {
        tool,
        mut processes,
        epochs,
    } = case(ExecutionLimits {
        deadline: EPOCH_TICK * DEADLINE_TICKS,
        ..ExecutionLimits::default()
    });

    // A trap after the spawn.
    let trapped = tool
        .execute(&call("announce:trap"), context(&CancellationToken::new()))
        .await;
    assert_ne!(trapped.status, ToolStatus::Ok, "{}", trapped.content);
    // Consume the announce spawn this call left in the fake's queue, so the later calls read
    // their own handles.
    wait_for_announce(&mut processes).await;
    let after_trap = processes.records().to_vec();
    assert_eq!(
        spawned_count(&after_trap),
        1,
        "a failed call was retried: {after_trap:?}"
    );
    assert!(settled(&after_trap), "{after_trap:?}");

    // A deadline after the spawn.
    let cancel = CancellationToken::new();
    let running = start(&tool, "announce:spin", &cancel);
    wait_for_announce(&mut processes).await;
    epochs.advance(u64::from(DEADLINE_TICKS));
    let late = running.await.expect("the executing task");
    assert_ne!(late.status, ToolStatus::Ok, "{}", late.content);
    let after_deadline = processes.records().to_vec();
    assert_eq!(
        spawned_count(&after_deadline),
        2,
        "a failed call was retried: {after_deadline:?}"
    );

    // The `touch` effect, then the trap that keeps it.
    let touched = trap_after_touch(&tool, &mut processes, "gone").await;
    assert_ne!(touched.status, ToolStatus::Ok, "{}", touched.content);
    let after_touch = processes.records().to_vec();
    assert_eq!(
        spawned_count(&after_touch),
        3,
        "a failed call was retried: {after_touch:?}"
    );
    assert_eq!(spawns_of(&after_touch, "touch gone"), 1, "{after_touch:?}");
    assert!(processes.markers().join("gone").is_file());

    // Nothing poisoned: the tool still answers a plain call.
    assert_usable(&tool, "three failed calls").await;
}

async fn an_unestablished_effect_is_never_success_and_carries_guidance() {
    // BLOCKERS S3-B3 (open, raised by the S3 lead): the S3.5 row asks that an effect that
    // cannot be established be answered `ToolStatus::Unknown` with reconciliation guidance,
    // but the frozen `ModuleFailure::into_tool_outcome` (S0, freeze item 5) answers `Error`
    // with "Effects before the failure may be partial; check the state before retrying" and
    // nothing in the runtime ever answers `Unknown` (there `Unknown` means "started before a
    // crash with no recorded result", set by the core on resume). S3 owns neither
    // `p1-module-protocol` nor `p1-module-runtime`, so this asserts the frozen behaviour:
    // never success, and the guidance the model reconciles the state from.
    let Case {
        tool,
        mut processes,
        ..
    } = case(ExecutionLimits::default());
    let outcome = tool
        .execute(&call("announce:trap"), context(&CancellationToken::new()))
        .await;

    assert_ne!(
        outcome.status,
        ToolStatus::Ok,
        "an unestablished effect answered success: {}",
        outcome.content
    );
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome
            .content
            .contains("Effects before the failure may be partial"),
        "no partial-effects warning: {}",
        outcome.content
    );
    assert!(
        outcome.content.contains("check the state before retrying"),
        "no reconciliation guidance: {}",
        outcome.content
    );
    // The effect was settled (the cases above prove it); only the answer's *status* is the
    // frozen `Error` rather than the row's `Unknown`, which is what S3-B3 decides.
    let records = processes.records().to_vec();
    assert!(settled(&records), "{records:?}");
}
