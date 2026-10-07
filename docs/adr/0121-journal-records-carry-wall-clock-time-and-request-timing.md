---
adr: 121
title: Journal records carry wall-clock time and request timing
status: proposed
date: 2026-10-07
deciders: lead
supersedes: []
superseded_by: []
sources: [crates/p1-journal/src/lib.rs, crates/p1-contracts/src/journal.rs, crates/p1-contracts/src/provider.rs, crates/p1-core/src/lib.rs, crates/p1-provider-http/src/drive.rs, ~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md]
---
# ADR-0121: Journal records carry wall-clock time and request timing

## Context
p1 journals record what happened but not when (issue #422). The 2026-09-27 harness analysis had to reconstruct per-turn latency from file mtimes and a `date` the model happened to run; the 2026-09-28 log scans could not measure per-turn wall clock, gaps, CI waits or provider back-off after five observed 429s. The later gap work (#418, #419, #421) promises latency effects that are measurable only if the journal carries time. Issue #63 asked for the same instrumentation earlier. Tests must use fake time (AGENTS.md).

## Decision
1. **Wall-clock time per record, stamped by the store.** The JSONL journal store (`p1-journal` `JsonlJournal`) writes `at_ms` (Unix milliseconds, from its clock) into each line it commits, beside the serialized `JournalRecord`. The `JournalRecord` type is unchanged, so existing readers and the ~77 record literals stay valid; a timed reader returns each record with its `at_ms` (`None` for lines written before this change). The store takes a clock (system by default, a fake one in tests).
2. **Request timing, measured by the core.** `p1-contracts` gains `Clock` (`fn now_ms(&self) -> u64`, a system implementation). The core holds one, the system clock by default, replaceable through `Agent::set_clock`. For every request it commits a new record `RequestTiming { request_index, sent_ms, first_event_ms, first_output_ms, ended_ms, waits }` immediately after that request's `AssistantCompleted` or `AssistantInterrupted`: `sent_ms` just before `Provider::stream`, `first_event_ms` at the first stream event of any kind (first-byte proxy), `first_output_ms` at the first text, reasoning or tool-input delta (first-token proxy; `None` if none), `ended_ms` at the terminal event. Resume and history projection ignore it.
3. **Provider waits.** `StreamEvent` gains `Wait { reason: WaitReason, attempt: u32, delay_ms: u64 }` (`WaitReason`: `rate_limited`, `server_error`, `transport`, `slow_first_byte`, `busy`). The host transport driver (`p1-provider-http` `drive.rs`) emits it where it already emits an operator `Notice` for a retry back-off or a slow first byte; the `Notice` stays. The core collects each `Wait` into the request's `RequestTiming.waits`; it is never history and never sent to the model.
4. **Tool time** follows from the store's `at_ms` on `ToolStarted` and `ToolFinished`; no new field.
5. **Report.** `scripts/journal-timing.py <journal.jsonl>` prints per request: gap since the previous record, time to first event, time to first output, decode time, waits, and the tool time and idle gap until the next request, from the journal alone; it says "unknown" for journals without times.

## Consequences
- A latency or stall question is answered from the journal: a stall between two records is visible as an `at_ms` gap; a 429 back-off as a `Wait`.
- Journals grow by a few dozen bytes per record.
- Every adapter that retries or waits should emit `Wait`; one that does not still gets `sent_ms`/`first_event_ms`, so its waits show as a long time to first event.
- Wall-clock time can jump; durations are computed within one run and a negative difference is reported, not hidden.

## Alternatives considered
- A new `at_ms` field on `JournalRecord`, set by the core: one clock for everything, but ~77 record literals across crates and tests change for a value the store can write.
- Monotonic offsets instead of Unix time: cannot be correlated with CI, cargo or other logs.
- A timing field on `AssistantCompleted`/`AssistantInterrupted`: ~50 literals change; a separate record keeps them as they are.
- A record per wait: commits in the middle of a stream; collecting waits into `RequestTiming` keeps the record order invariants unchanged.

## Evidence
- `~/.agents/xo/dispatch/cutover-lead/analyst/REPORT.md` §2.2 (decomposition reconstructed without timestamps); `logscan/a/SUMMARY.md` (unmeasurable gaps, 5 × 429).
