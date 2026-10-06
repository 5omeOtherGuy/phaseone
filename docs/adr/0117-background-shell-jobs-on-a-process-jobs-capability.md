---
adr: 117
title: Background shell jobs on a process-jobs capability
status: proposed
date: 2026-10-05
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0001, ADR-0035, ADR-0051, ADR-0109, issue #514, audit slice G, DECISIONS D30 D31]
---
# ADR-0117: Background shell jobs on a process-jobs capability

## Context

A `shell` call blocks the turn until its command ends. A long build or test run holds the
model idle, and the shell timeout forces a choice between a long wait and a killed command.
Issue #514 (tools slice G of the iris-tools audit, `~/.agents/xo/dispatch/p1-iris-tools-audit/AUDIT.md`
sections 4.3 and 5.G) asks for `shell {…, background?: bool}` and
`shell_job {job_id, action: "status" | "cancel"}`. The model learns that a job ended from a
host notification, never by polling, and reads its output through the output store (ADR-0109,
`read_output`). Persistent shell sessions, job lists and multiplexing stay out.

`process.running` (`modules/wit/process.wit`) cannot outlive the call that started it: the host
drops it, and kills its process group, when the call returns. The core already delivers
"something you wait for has finished" messages: `Inbox::send(InboxKind::Notification, …)`
(`crates/p1-core/src/lib.rs`), used for delegated workers (`crates/p1-host/src/catalog/children.rs`).
Finish verification (ADR-0051) counts an `executes` call with exit code 0 recorded after the
last file change (`crates/p1-host/src/activity.rs`). The owner ordered tools E–J after #575 on
GPT-6.1 Sol high workers (D30, D31).

## Decision

- **A new interface, `process-jobs`**, in a new `modules/wit/jobs.wit`, allocated to the `tool`
  class only and granted only to `p1/shell` (start) and `p1/shell-job` (status, cancel).
  Additive as ADR-0109, ADR-0115 and ADR-0116: the package stays `p1:module@1.0.0`.

  ```wit
  interface process-jobs {
      use process.{exit-status};
      variant job-state {
          running(job-progress),
          ended(job-end),
      }
      record job-progress { elapsed-ms: u64, output-bytes: u64 }
      record job-end {
          status: exit-status,
          elapsed-ms: u64,
          /// The `tool-outputs` handle holding everything the job printed.
          output: string,
      }
      variant job-error { unknown-job, start-failed(string) }
      /// Start `script` as `bash -lc`; returns the job id at once. `timeout-ms` none: the job
      /// runs until it ends, is cancelled or the session ends.
      start: func(script: string, timeout-ms: option<u64>) -> result<string, job-error>;
      status: func(job-id: string) -> result<job-state, job-error>;
      cancel: func(job-id: string) -> result<job-state, job-error>;
  }
  ```

- **The host owns every job**, in a per-session job registry in `p1-module-runtime` beside
  the process service, never in the guest's call. A job runs exactly as `process.spawn` runs a
  command (ADR-0035): same sandbox decision, environment rebuilt from the allow-list, own
  process group, workspace root, no terminal, no input. Ids are `j1`, `j2`, … per session.
  A job belongs to the session that started it; any other id, including another session's
  or a delegated worker's, is `unknown-job`. Cancel and timeout kill the job's process group;
  ending or dropping the session kills every job it still runs, and no process of the group
  survives. No default deadline is added.
- **Output goes to the output store** (ADR-0109) as it arrives, redacted before storage, under
  one handle per job, with the same complete/incomplete capture marks. Status and the end
  notification report the handle; reading it consumes nothing.
- **The end is recorded once, then announced.** When a job ends, the registry fixes its
  `job-end`, then the host sends one `InboxKind::Notification` to the owning agent: job id,
  command, exit status, elapsed time, output byte count, the handle, and the last 2,000 bytes
  of its stored (redacted) output. Status after the notification returns the same `job-end`.
- **Finish verification counts a job at its real end, for the files it ran against.** The
  `shell` call that starts a job records no exit code, so it never counts. The registry notes
  the session's last file-change order when the job starts. When the job ends, the host records
  a finished `executes` run with the job's command; its order is assigned at that moment, and
  its exit code is the job's only if the job exited 0 and no file changed between its start and
  its end; otherwise it records no successful run. A failed, timed-out or cancelled job records
  no successful run. Job ends are not replayed on resume: after a restart a verification command
  must be run again.
- **Tools.** `shell` gains `background?: bool = false`; with `true` it calls `start` and returns
  the job id and how to check it, and applies no output filter (the output is in the store).
  `p1/shell-job` (`p1-tool-shell-job` guest) maps `{job_id, action}` to `status` or `cancel` and
  formats the state. `shell` keeps its `executes` effect; `shell_job` is `read-only` for status
  and `executes` for cancel, since cancel ends a process.

## Consequences

- A model can start a long build, keep working, and be told when it ends, with the full output
  recoverable and the end counted as verification only when it really succeeded.
- One more interface, one more tool component, a host registry, a guest change to `shell`; the
  allocation table and the boundary check change.
- A job's success is not evidence after a resume; the model runs the check again.
- Whether background jobs save turns or tokens is unmeasured until the issue's five long-build
  tasks run.

## Alternatives considered

- **Reuse `process.running` past the call**: it is call-scoped by contract; widening it changes
  every tool that spawns a command.
- **Status polling without notification**: costs turns and tokens and is what the issue
  excludes.
- **Persistent shell sessions**: excluded by the issue; they keep state the host cannot bound.

## Evidence

- Process contract: `modules/wit/process.wit` (`running`, drop kills the group).
- Notification path: `crates/p1-core/src/lib.rs` (`Inbox`), `crates/p1-host/src/catalog/children.rs`.
- Verification rule: ADR-0051, `crates/p1-host/src/activity.rs` (`record_finished_with_exit`,
  order assigned at record time).
- Output store: ADR-0109, `modules/wit/outputs.wit`.
- To re-check after implementation: start returns at once; notification and later status agree;
  a running or failed job never verifies; a success counts at its end order only when no file changed while it ran; cancel touches only
  the session's own job; zero processes in the group after session end; the measured
  foreground-vs-background table on five long-build tasks.
