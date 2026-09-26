# Cancellation and the restricted path

Status: published freeze item 4 of the WebAssembly boundary (ADR-0071): how a module call is
bounded and cancelled, the rule that a trap never undoes a native effect, and how the
synchronous `effect` and `describe` calls enter a restricted execution path (F10). The WIT side
of the rule — what `control.cancelled()` and the blocking imports promise a module — is in
[`wit.md`](wit.md#cancellation-and-the-restricted-path-freeze-item-4-the-wit-part); the
streaming-resource contract of item 10 is in
[`wit.md`](wit.md#streaming-resources-freeze-item-10). This document states what the runtime
crate [`crates/p1-module-runtime`](../../../crates/p1-module-runtime/) does. The decision behind
it is [ADR-0082](../../adr/0082-component-abi-and-execution-ownership.md).

## Two paths into a module

| Path | Exports | Where it runs | File |
|---|---|---|---|
| Execute | a tool's `execute` (and, with S4 and S5, the asynchronous exports of the other worlds) | the module's one executor task, a fresh Store and instance per call, every granted capability linked | [`executor.rs`](../../../crates/p1-module-runtime/src/executor.rs) |
| Restricted | a tool's `declaration`, `effect`, `describe`, `describe-result` | the caller's own thread, a second instance behind a mutex, no capability linked | [`restricted.rs`](../../../crates/p1-module-runtime/src/restricted.rs) |

Both use the one engine of [`lib.rs`](../../../crates/p1-module-runtime/src/lib.rs)'s `engine()`,
built with the component model, epoch interruption and fuel consumption on. The engine's epoch
clock is advanced by a ticker thread the `Loader` starts (`EPOCH_TICK` in
[`loader.rs`](../../../crates/p1-module-runtime/src/loader.rs)); a test drives it by hand through
`Loader::with_manual_epochs`, so no test waits on a real clock.

## The execute path

Every call gets a fresh Store and a fresh instance from the pre-linked component, with the
limits of `ExecutionLimits` (`fuel`, `deadline`; the defaults are `DEFAULT_FUEL` and
`DEFAULT_DEADLINE` in `executor.rs`). Three mechanisms bound it:

- **Fuel.** The Store starts with the call's fuel; running out is the trap `OutOfFuel`, mapped to
  `ModuleFailure::FuelExhausted`. Fuel counts guest instructions only — a host wait costs none —
  so only a runaway computation meets it. The guest also yields to the Tokio scheduler every
  `FUEL_YIELD_INTERVAL` of fuel, so a busy guest never starves the caller's runtime (a
  current-thread one included), and the call's task keeps watching its deadline and its
  cancellation while the guest computes. Every call starts with the full budget again.
- **Epoch deadline.** The deadline is counted in epoch ticks and bounds the whole call, host
  waits included. While guest code runs, the Store's epoch callback returns an error once the
  deadline has passed; while the guest waits in a host import, the call's task sees the clock
  pass the deadline and abandons the call. Either ends as `ModuleFailure::DeadlineExceeded`.
- **Cooperative cancellation.** When `ToolContext.cancel` fires, `control.cancelled()` answers
  true for the rest of the call, every blocked host import returns as its WIT contract says
  (a blocked `process.running.next` returns what remains and then `exited(cancelled)`), and the
  call's task interrupts the engine's epoch once, so a guest in a CPU loop reaches its epoch
  callback at once. The callback cuts the call's remaining fuel to `CANCEL_GRACE_FUEL`: enough
  to serialize a `cancelled` outcome and return cleanly, and a loop that never checks runs out
  of it. A cancelled call that exhausts its grace is answered `ModuleFailure::Cancelled`, not a
  fuel or deadline failure. A call whose token is already cancelled does not start.

`ModuleFailure` maps into the closed `ToolOutcome` and `ProviderErrorKind` shapes of
[`protocol.md`](protocol.md#error-mapping-freeze-item-5); cancellation adds no kind.

### Abandoning a call

The caller's future holds no Store: it holds the reply end of a channel. When the caller drops
it, the executor's task for that call sees the reply channel close and returns, dropping the
call's future and with it the Store. Dropping the Store drops every resource the call held, so a
`process.running` the guest still held ends its process group
([`capabilities.rs`](../../../crates/p1-module-runtime/src/capabilities.rs) drops the service's
handle, and the `ProcessService` contract requires that dropping a running handle kills the
group). The same happens when a call ends by a deadline, a trap or a normal return: a resource
never outlives the export call that created it.

### A trap never undoes a native effect

Nothing the host did for a call is rolled back when the call traps, runs out of fuel, passes its
deadline, is cancelled or is abandoned: a command that ran, a file a command wrote or a worker
that started stays done. The runtime has no journal of effects to reverse and pretends to none;
a tool error text says which failure happened and that effects before it may be partial
([`protocol.md`](protocol.md#error-mapping-freeze-item-5)). Because every call has its own Store
and instance, what a failed call leaves behind is limited to those native effects: the next call
starts from a fresh instance.

## The restricted path (F10)

`p1_contracts::Tool`'s `effect`, `describe` and `describe_result` are synchronous, and the host
calls them from inside async code on any Tokio flavour. They must never wait on the executor or
block on a runtime, so they do not use the execute path:

- They run on a **second instance** of the module that the adapter owns behind a `Mutex`, called
  on the caller's own thread.
- **No capability is linked**: the restricted linker defines every import with
  `define_unknown_imports_as_traps`, so an import called there traps and inspection code cannot
  reach a capability.
- The call is bounded by `RESTRICTED_FUEL` and, as a backstop behind the fuel, the epoch deadline
  `RESTRICTED_DEADLINE_TICKS`; each call starts with the full budget.
- The restricted Store never sees an asynchronous definition, so the call uses wasmtime's
  synchronous `Func::call`: no fiber, no future and no poll loop. Were an asynchronous definition
  ever added there, wasmtime would refuse the synchronous call loudly instead of the path
  degrading into a spin.
- A trap leaves an instance that may not be entered again, so it is dropped and the next call
  builds a fresh one.
- A failed inspection degrades to the worst case, never a panic: a failed `effect` is
  `Effect::Executes`, a failed `describe` is the empty description with the verb `call`, and a
  failed `describe_result` is the host's own first-line summary
  ([`tool.rs`](../../../crates/p1-module-runtime/src/tool.rs)).

What only a capability can know — whether a path escapes the workspace through a symlink — is
judged lexically on the restricted path and enforced again by the capability when the call
executes. A world whose restricted export depends on settings (a provider's `describe`) gets
`configure` called first on the restricted instance
([`wit.md`](wit.md#cancellation-and-the-restricted-path-freeze-item-4-the-wit-part)); the tool
world has no `configure`.

## Tests

Every case runs on a current-thread and a multi-thread Tokio runtime under the harness's deadlock
guard ([`crates/p1-module-tests/src/lib.rs`](../../../crates/p1-module-tests/src/lib.rs)), over
the fixture component `p1/fixture` whose modes are listed in
[`modules/p1-module-fixture/src/lib.rs`](../../../modules/p1-module-fixture/src/lib.rs).

[`runtime_spike.rs`](../../../crates/p1-module-tests/tests/runtime_spike.rs)
(`cargo test --locked -p p1-module-tests --test runtime_spike`):

- `inspection_is_synchronous_inside_a_running_task` — `effect`, `describe` and `describe_result`
  answer on the spot inside a spawned task; `describe-import` calls an import on the restricted
  path, traps, and yields the empty description; the next call works on a rebuilt instance.
- `execute_runs_each_call_on_a_fresh_instance`, `execute_awaits_an_async_host_import`,
  `concurrent_calls_all_complete` — the executor's per-call instances and asynchronous imports.

[`cancellation.rs`](../../../crates/p1-module-tests/tests/cancellation.rs)
(`cargo test --locked -p p1-module-tests --test cancellation`):

| Case | What it shows |
|---|---|
| `an_epoch_deadline_stops_a_cpu_loop` | a CPU loop ends `DeadlineExceeded` past the deadline and not before; the resource the guest held is dropped with the call |
| `a_small_fuel_budget_stops_a_cpu_loop` | a loop ends `FuelExhausted`; the next call starts with the full budget |
| `a_cooperative_guest_returns_cancelled` | a guest checking `control.cancelled()` returns its `cancelled` status |
| `cancellation_interrupts_a_cpu_loop` | with no epoch advanced, only the cancellation interrupt and the grace fuel end a loop that never calls an import, as `Cancelled` |
| `a_cancellation_during_spawn_ends_the_call_cancelled`, `a_refused_cancelled_spawn_ends_the_call_cancelled` | a cancellation while the guest is blocked in `process.spawn` reaches the guest as `exited(cancelled)` on the resource, never as a `spawn` error |
| `cancellation_wakes_a_blocked_next` | a `next` blocked on a host wait returns after the host kills the process group, then `exited(cancelled)` |
| `dropping_the_call_while_next_is_blocked_kills_the_process` | abandoning the call while `next` is blocked drops the resource and kills the process |
| `a_guest_drop_kills_the_process` | the guest dropping its resource ends the process |
| `next_after_the_terminal_event_traps_and_keeps_the_effect` | `next` after `none` traps, and the command's native effect (a marker file) stays |
| `a_trapped_or_cancelled_call_poisons_nothing` | after a trap, a deadline or a cancellation the tool keeps answering |

Both test files run in `scripts/gate.sh` on the box and in CI. The recorded runs are S0.5's
(PR #252, merge commit `4872fd78`) and S0.6's (PR #272, merge commit `a808a2ed`).
