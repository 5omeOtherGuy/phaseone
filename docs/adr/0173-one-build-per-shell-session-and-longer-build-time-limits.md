---
adr: 173
title: One build per shell session and longer build time limits
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: []
---
# ADR-0173: One build per shell session and longer build time limits

## Context

Issue #77, from the 2026-09-23 session-log scan (`docs/research/session-log-scan-2026-09-23.md`). Three workspace-wide cargo commands burned 10 200 s in timeouts in one run; two cargo runs in one session deadlocked on cargo's build lock and the machine's `rustc-serial` slots for 2 × 3 600 s; a full gate does not fit 3 600 s on a loaded machine. The owner trimmed the issue on 2026-10-11 to two items: recognised build and test commands (`cargo …`, `scripts/gate.sh`) get a longer limit, default 1 800 s and maximum 7 200 s, and the tool description says so; a second `cargo`/`gate.sh` run in the same session while one is still running is refused at once, naming the running call, with background jobs counting as running and other parallel calls still allowed.

The shell guest (`p1-shell-guest`) is pure computation and runs once per call, so it cannot remember what earlier calls started. Since ADR-0123 a timed-out call keeps running as a background job, and a build call is never a read, so it never runs beside another call of its agent (ADR-0118). In the component, the builds that can still be running when a build call starts are therefore the session's background jobs, and the host's job registry is the one place that knows them.

## Decision

1. **Recognition is the guest's.** `p1-shell-guest` classifies a command line as a build when any simple command of it (after assignments, `env`/`command`/`nohup`/`sudo`, `time`, `nice` and `timeout` prefixes; here-document bodies are text, not commands) runs `cargo` with any subcommand except `fmt`, runs a file named `gate.sh`, or runs `bash`/`sh` on either. `cargo fmt` neither compiles nor takes the build lock; `cargo --version`, `cargo metadata` and the like count as builds, as the owner's "`cargo …`" says.
2. **Longer limits for builds.** A build defaults to 1 800 s and accepts `timeout_seconds` up to 7 200 s; every other command keeps 120 s and 3 600 s. The tool description and the `timeout_seconds` description state both. The input schema's own `default` and `maximum` stay 120 and 3 600, the values for a command that is not a build.
3. **One build per session.** A build call while another build of the session is still running is refused at once with status error. The refusal names the running build, as `call <id>` or `background job <id>`, and its command's first line. A call that is not a build is never refused. The guest decides (`build_refusal`); each host only says what is running.
4. **The component learns the running jobs through one additive function.** `process-jobs` gains `record running-job { job-id: string, command: string }` and `running: func() -> list<running-job>`: the session's jobs still running, oldest first, each with the command it was started or handed over with. The package stays `p1:module@1.0.0` (additive, as ADR-0117 and ADR-0123); a component built without it still loads. The host's shell and `shell_job` services answer from the session's `JobRegistry`.
5. **Native adapter.** `ShellTool` has no jobs; it holds its own running build calls and passes them to the same guest decision, so its tests prove the refusal the component ships.
6. **The shell's call deadline covers its longest limit.** The host ran `p1/shell` under the module default call deadline of 300 s, which counts host waits, so any foreground command was cut off at 300 s whatever its `timeout_seconds`: the call failed with the module time-limit error and the command died with it, never handed over. The host now gives the shell a call deadline of the guest's longest `timeout_seconds` (`LONGEST_TIMEOUT_SECONDS`, 7 200 s) plus a minute (`shell_limits` in `crates/p1-host/src/catalog/tools.rs`); fuel still bounds the guest's own computation, and cancellation is unchanged.

## Consequences

- A model that starts `cargo test` in the background and then runs `cargo build` gets an immediate refusal naming the job instead of a call that waits on the build lock until its timeout.
- A long build no longer needs an explicit `timeout_seconds` to finish in the foreground; a model that waits on a build blocks for up to 30 minutes by default before the command is handed over to a job (ADR-0123).
- An explicit `timeout_seconds` above 300 now holds for every command; before, the module deadline silently ended such a call at 300 s.
- The refusal is per session (one job registry): another agent's build, such as a delegated worker's, is not refused; the machine-wide limit stays `rustc-serial`'s.
- A schema-validating client sees a maximum of 3 600 s; a build that needs more relies on the description.
- Not in scope: a `rustc-serial` slot hint on timeout and a cargo summariser that keeps the last `Compiling`/`Running` line (issue #77 items 3 and 4).

## Alternatives considered

- **Recognise builds in the host's process service.** Rejected: the host would have to know commands (ADR-0123 rejected that too), and the tool's behaviour would live outside the code its guest and native adapter share.
- **A build lock taken at spawn time.** Rejected: `process.command` is a frozen record, and a lock would block a second build instead of refusing it at once.
- **Raise the limits for every command.** Rejected: a hung command that is not a build would then block a turn for up to two hours.
- **Pause the call deadline while the shell waits on its command**, as an operator question pauses it (ADR-0116). Not chosen here: it changes the runtime's deadline rule for every module with the `process` grant, where a larger deadline for the one shell registration is local; the command's own limit already bounds the wait.

## Evidence

Issue #77 and its owner scope of 2026-10-11. Tests: `crates/p1-tool-shell/guest/src/build.rs` (classification), `crates/p1-tool-shell/guest/src/lib.rs` `a_build_gets_the_longer_time_limits` and `a_second_build_is_refused_naming_the_running_one`, `crates/p1-tool-shell/src/lib.rs` `a_second_build_while_one_runs_is_refused_naming_the_running_call` and `a_build_accepts_the_longer_time_limit_and_other_commands_do_not`, `crates/p1-module-runtime/src/jobs.rs` `running_lists_the_jobs_still_running_with_their_commands`, `crates/p1-host/src/catalog/tools.rs` `a_shell_call_may_wait_as_long_as_its_longest_timeout`, and the component path `crates/p1-module-tests/tests/shell_build_runs.rs` (the refusal naming a background job; under manual epochs, a foreground build cut off at the default 300 s deadline and kept alive under the shell's deadline).
