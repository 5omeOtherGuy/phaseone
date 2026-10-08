---
adr: 123
title: A timed-out shell command continues as a background job
status: proposed
date: 2026-10-07
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0123: A timed-out shell command continues as a background job

## Context

Issue #456. The model must guess `timeout_seconds` for every shell call. When the guess is short, the command is killed, its work is lost and it is run again; when it is long, the turn blocks for the whole time. The 2026-09-28 log scan counted 21 shell timeouts in 6 of 8 cutover runs, limits from 20 s to 1 200 s, most of them cargo builds or tests waiting on the shared build lock (`runs/j4-p1.jsonl` seq 147 `[timed out after 1200 s]`; `runs/j5-p1.jsonl` seq 504; `runs/j2-p1.jsonl` seq 294).

ADR-0117 gave p1 background jobs: `shell` with `background: true` starts a session-owned job `jN` in the host's job registry, its output goes to the output store, its end arrives as one inbox notification, and the run's end kills every job still running. A foreground call cannot become one today. Its process stream carries the call's cancel token and a watchdog that kills the process group at `timeout_ms` (`crates/p1-module-runtime/src/process/stream.rs`), and the `process` interface promises that no command outlives its call.

## Decision

A foreground `shell` command that reaches its `timeout_seconds` is not killed. The host hands it over to the job registry as a background job, and the call returns at once with that job's id and the output so far.

1. **Handover in the host.** The foreground deadline is no longer the stream's kill deadline. The host races it beside the stream; when it fires and the command still runs, the host adopts the running process, its output-store entry and its group into the session's job registry under the next id (`JobRegistry::adopt`, sharing the code `start` uses from the spawn onward). From then on it is a background job in every respect of ADR-0117: its own cancel token, no deadline, `shell_job` status and cancel, one completion notification, killed when the run (or a delegated worker's turn) ends.
2. **Cancellation before the deadline is unchanged.** A turn cancelled while the command is still a foreground command kills its group, as today.
3. **The guest learns it through one additive function.** `process-jobs` gains `handed-over: func() -> option<string>`: the job id the host gave the calling tool's timed-out command, if it handed it over. The stream still ends the call with `exited(timed-out)`; the shell guest then asks `handed-over` and, given an id, returns status Ok with: `still running as background job jN after N s; its completion arrives as a notification; shell_job checks or cancels it` followed by the output so far. Without an id (a tool without the grant) it renders `[timed out after N s]` as today. The package stays `p1:module@1.0.0` (additive, as ADR-0117).
4. **Finish evidence.** The foreground call records no exit, so it never counts. The adopted job's baseline is the file-change order at the handover; its end counts under ADR-0117's rule (exit 0 and no file changed between the handover and its end).
5. **Description.** The shell description says a command that outlives `timeout_seconds` continues as a background job, and drops "on timeout ... the whole process group is killed".
6. **Native tool.** The in-process `ShellTool` (`crates/p1-tool-shell/src/lib.rs`), which has no job registry, keeps killing on timeout; the shipped `shell` is the component.

## Consequences

- A command that needs longer than the guess finishes once, as a job, instead of being killed and repeated. A model that guessed long still blocks for its own guess.
- Expected values change in the tests that assert a foreground timeout kills the group through the component path (`crates/p1-module-tests/tests/shell_boundary.rs` `a_timed_out_call_takes_its_descendants_with_it`): the descendant now survives as job `j1` until it ends or the session ends. Stream-level kill tests stay, because a background job's own deadline and the session end still kill.
- A genuinely hung command now runs until the run ends instead of until its timeout. Delegated workers' jobs still end with the worker's turn.
- `process.wit`'s doc comment changes: a command outlives its call only by being handed over to a job.

## Alternatives considered

- **A new `exit-status` case `handed-over(string)`.** Rejected: `exit-status` is a frozen variant shared with `jobs.wit`; changing it breaks components built against 1.0.0.
- **Keep the kill and only warn the model to guess longer.** Rejected: that is today's behaviour, and the evidence is 21 lost runs.
- **Hand over only above a size or for named commands.** Rejected: the host would have to know commands; every timeout is handed over.

## Evidence

Issue #456 and the 2026-09-28 log scan (`~/.agents/xo/dispatch/cutover-lead/logscan/`). Code survey: `crates/p1-module-runtime/src/process/{mod.rs,stream.rs,capability.rs}` (deadline, watchdog, call token), `crates/p1-module-runtime/src/jobs.rs` `start` (adoptable from the spawn on), `crates/p1-module-runtime/src/capabilities.rs` `HostRunning`, `modules/wit/process.wit` and `modules/wit/jobs.wit`, `crates/p1-host/src/activity.rs` `job_started`/`job_finished`.
