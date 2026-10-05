---
adr: 114
title: A workflow run ends when the process outgrows its memory budget
status: proposed
date: 2026-10-05
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [ADR-0053, issue #575, issue #541 E15, PR #574 review, DECISIONS D30]
---
# ADR-0114: A workflow run ends when the process outgrows its memory budget

## Context

Rhai's data limits apply to one value: at most 4 MiB of strings, 262,144 array items and
262,144 map entries in each (`engine.rs` `data_limits`). A script keeps up to 256 variables and
runs up to eight thunk threads (`MAX_RUN_THREADS`), and doubling a string or an array costs a
few operations, so the per-value limits multiply to several GiB and `max_operations` is no
aggregate bound (#541 finding E15). The workstation has 11 GiB usable memory and three builds
may run beside p1.

PR #574 tried a run-wide "data fuel" that charged every variable read and the whole scope on
every runtime `let`. Legal scripts died: one 64 KiB envelope read 4096 times exhausted 256 MiB,
and a value nested deeper than 32 levels ended the run (review of #574). Rhai 1.26.1 offers no
hook that reports allocations or retained data: `on_var` runs before an access and gets no
value, `on_def_var` runs before the initializer, `on_progress` gets only an operation count
(source check by the #574 repair worker, issue #575). The fuel was removed and E15 moved to
#575. The owner chose "#575 first, then tools" on 2026-10-05 (D30).

## Decision

A workflow run measures what it costs the process instead of counting script values. At run
start it records the process's resident memory; the engine's existing `on_progress` hook (which
already ends a cancelled run) reads the monotonic clock on every operation and, at most once per
millisecond, the process's current resident memory. When that exceeds the run's start value by
the budget — 1 GiB, a constant — the hook ends the evaluation the way cancellation does: the
script cannot catch it, every thunk of the run stops at its next operation, and the run ends
`failed` with "the workflow run exceeded its memory budget (1 GiB above the process at start)".

- Resident memory is read from `/proc/self/statm` (Linux, std only). Where it cannot be read
  (another OS, or the file is unavailable), there is no aggregate bound and the per-value limits
  remain; the run never fails for want of the reading.
- The reader is a seam (a function the engine is given), so tests drive the bound with
  deterministic values; one integration test also grows a real script past a test budget.
- The per-value limits, `max_operations`, `max_variables` and the thread clamp stay as they are.

## Consequences

- A runaway script is stopped within about a millisecond of memory traffic past the budget plus
  one operation's allocation (one capped value, at most tens of MiB), long before the machine
  swaps. Reads and loops cost nothing against the budget; only memory the process actually holds
  counts, so the #574 false failures cannot recur.
- The measure is process-wide. Another run, a worker or the host growing in the same process
  during a run counts against it, so concurrent runs can end each other once the process has
  grown by 1 GiB. That is accepted: by then the process is the risk, whichever part grew.
- Memory a script frees is not "refunded" by the allocator at once, so resident memory may stay
  high after a peak; the bound follows what the process holds, which is what the machine pays.
- One clock read per operation, beside the cancellation check every operation already runs. Its
  cost is unknown; the implementing PR measures a CPU-bound script's run time with and without
  the check. Scripts spend most of their time blocked in `agent()`, which runs no operations.
- No child process, no new dependency, no unsafe code.

## Alternatives considered

- **Run-wide data fuel charged on reads and definitions** (#574): ended legal scripts; removed.
- **Script evaluation in a child process with an address-space or cgroup limit**: a real
  per-run bound, but `agent()` blocks on the host's runtime and journal, so every host call would
  need an IPC protocol; far larger than the gap it closes.
- **Lower per-value limits, variables and threads until their product is safe**: 23 MiB × 256 ×
  8 would have to shrink by two orders of magnitude, which breaks the documented fan-out sizes
  (64 full envelopes per value).
- **A counting global allocator**: exact, but needs `unsafe` (`GlobalAlloc`), which the
  workspace forbids outside ADR-0113's one call.
- **Machine `MemAvailable` floor instead of process growth**: protects the machine, but a build
  elsewhere would fail unrelated workflow runs.

## Evidence

- Rhai hook signatures: `rhai-1.26.1/src/api/events.rs:60-68, 242-249`, `src/eval/stmt.rs:401-425`,
  `src/types/var_def.rs:8-50` (cited in #575 from the #574 repair worker's report).
- #574 review false failures (recomputed by the reviewer): 4096 reads of a 64 KiB envelope
  exhaust 256 MiB; eleven 30 KiB envelopes and 800 `let x = i;` iterations charge 270,336,000
  bytes while 337,920 are retained.
- To re-check after implementation: the regression tests named in the implementing PR, and
  `cargo test -p p1-workflow`.
