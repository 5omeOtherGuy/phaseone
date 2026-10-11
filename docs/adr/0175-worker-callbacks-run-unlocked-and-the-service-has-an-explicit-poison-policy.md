---
adr: 175
title: Worker callbacks run unlocked and the service has an explicit poison policy
status: accepted
date: 2026-10-11
deciders: lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/67, docs/adr/0053-workflows-are-an-optional-module-a-sandboxed-script-orchestrates-workers-under-roles-and-caps.md, docs/adr/0161-shell-diagnostics-and-single-worker-result-delivery.md, crates/p1-workers/src/lib.rs]
---
# ADR-0175: Worker callbacks run unlocked and the service has an explicit poison policy

## Context

Issue #67: `InProcessWorkers` ran the host's child build (the prepared start of
ADR-0053 item 2) and the re-grant callback (ADR-0050 item 6) while it held its one
state mutex, and read every mutex with `lock().unwrap()`. A panic in either callback
poisoned that mutex, and the next `unwrap()` in any call took down the whole worker
service, every other worker with it. ADR-0053's guard already turns a panic inside a
turn into `Failed`; the callbacks had no such handling.

The host's direct factory predicts the next worker id from its own counter and
relies on its calls never overlapping, so the callbacks cannot simply run in
parallel.

## Decision

1. **No lock held across a host callback.** A child's build and a re-grant run with no
   service lock held. They are serialised by one async build lock, which keeps the
   host's id prediction right. A start reserves its running slot in the service state
   before it builds and publishes the child afterwards, so the running count and the
   id assignment stay atomic. The state lock is released during the build, so every
   other worker stays observable and controllable.
2. **A panicking callback is a clean error.** A panicking build is a failed start,
   `InvalidEnvironment("building worker wN panicked")`, and it consumes nothing,
   exactly like a build that returns an error. A panicking re-grant refuses the
   continue with `Regrant("re-assembling worker wN panicked")` and runs no turn. It
   leaves that worker `Failed`, unless a newer status replaced the one the re-grant
   was asked against, and holds no slot; a later continue may try again. In both
   cases the next `start` succeeds.
3. **Poison policy.** Every mutex in `p1-workers` is one of two kinds.
   - *Value locks* are read through poison with `PoisonError::into_inner` (the
     `relock` helper). Each write to one replaces or adds one whole value, so a
     poisoned value is still valid. They are the parent and per-agent inboxes, the
     result tool name, a turn's cancellation token and stall reason, a worker's result
     receipt (each critical section ends with its one inbox call, so an interrupted
     section loses at most that notice), the task handles, and the scope registry
     and members.
   - *The service state* has invariants that span several fields: the running count,
     the ids, and which child is `Running`. After a panic it is never trusted again.
     Every call returns `WorkerError::ShutDown`, `list` is empty and `running`
     reports the service full. Only teardown (`shutdown`, `Drop`) reads through
     the poison, to cancel every turn. A child task that ends while the state is
     poisoned still publishes its terminal status on its watch, so no waiter is
     left parked; a result wait that was already parked ends with `ShutDown`.

The `WorkerError` enum is unchanged.

## Consequences

One misbehaving factory or re-grant cannot stop the other workers, and callers see
a `WorkerError` instead of a propagated panic. Builds remain serialised, as they
were under the state lock, so concurrent starts are no faster. A start that
waits for another start's build now yields instead of blocking a thread. A
build that finishes after `shutdown` began is dropped with `ShutDown`; it no longer
starts behind the shutdown. Once the state is poisoned, which only a defect in
`p1-workers` itself can now cause, the service fails closed as if shut down.

## Alternatives considered

- A build panic returned as `Ok(id)` with a `Failed` worker. Rejected: a start
  answers `Ok` only once a real child exists (seam #715), and consuming the id would
  break the host's id prediction.
- A new `WorkerError` variant for poison. Rejected for now: the variant set is
  mirrored by the module runtime's WIT mapping, and `ShutDown` already tells a
  caller the truth: the service can no longer serve.
- Recovering the state lock through poison. Rejected: a panic between publishing
  `Running` and sending the turn would leave a child counted as running forever.

## Evidence

The `p1-workers` unit tests:
`a_panicking_build_fails_its_start_and_the_next_start_succeeds`,
`a_panicking_factory_fails_one_start_and_the_next_start_runs`,
`a_panicking_regrant_fails_that_worker_and_the_next_start_succeeds`,
`a_panicking_regrant_never_overwrites_a_newer_status`,
`the_build_and_the_regrant_run_with_the_state_lock_free`,
`a_poisoned_state_is_a_shutdown_error_and_teardown_still_cancels` and
`value_locks_read_through_poison` (`cargo test -p p1-workers`).
