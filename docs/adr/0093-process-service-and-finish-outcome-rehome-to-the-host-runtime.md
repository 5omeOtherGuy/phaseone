---
adr: 93
title: Process service and finish outcome rehome to the host runtime
status: proposed
date: 2026-09-27
deciders: lead
supersedes: []
superseded_by: []
sources: [issue #391 (S7.10-R3), ~/.agents/xo/reports/cutover-fallbacks/PLAN.md rows "p1-tool-shell · S3" and "p1-tool-finish · S3", ADR-0071, ADR-0083, PR #382 (S3.8)]
---
# ADR-0093: Process service and finish outcome rehome to the host runtime

## Context

Since S3.8 (PR #382, ADR-0083) the host assembles `shell` and `finish` only as the
components `p1/shell` and `p1/finish`. Yet `p1-host` still depended on the native tool
crates `p1-tool-shell` and `p1-tool-finish` in its normal graph, for services the host
itself provides to those components: the native process service, its bubblewrap sandbox and
the adapter that backs the `process` capability (`ProcessService`, `Sandbox`,
`ProcessCapability`), the sandbox presentation (`SANDBOX_PARAGRAPH`,
`SANDBOX_VARIANT_SUFFIX`), and the accepted-outcome cell the completion hub writes
(`FinishOutcome`) together with the `SessionActivity` trait the host's record implemented.
The S7.10 cutover removes native extension crates from the host's normal graph (ADR-0071).

## Decision

The native process service, sandbox and `process` capability adapter move to
`p1_module_runtime::process`, the host runtime that serves the capability; `p1-tool-shell`
re-exports them for its native tool. The sandbox paragraph and variant suffix move to the
shell's contract crate `p1-shell-guest`. The host's accepted `finish` cell, `FinishOutcome`,
is `p1_host::activity::FinishOutcome`, written only by the completion hub; the session record
(`ActivityLog::last_file_change`, `ActivityLog::shell_runs`) is inherent to the host, and
the shared finish types come from `p1-finish-guest`. `p1-tool-finish` keeps its own
`FinishOutcome` and `SessionActivity` for its native adapter. Both tool crates are
dev-dependencies of `p1-host`.

## Consequences

`cargo tree -p p1-host -e normal` no longer contains `p1-tool-shell` or `p1-tool-finish`.
Process-group cleanup, environment filtering, sandbox grants and the presentation text are
the same code in a new crate, so their tests moved with it (unit tests) or keep running
through the re-exports (`p1-tool-shell/tests`, `p1-module-tests`). `p1-module-runtime` gains
`nix`, `tempfile` and tokio's `process`, `io-util` and `time` features, which were already in
the workspace. The native `finish` adapter and the host now have separate outcome cells:
a test that feeds the host's cell from the native tool commits what the tool accepted, as
the hub commits a verified candidate. `ProcessService::run_until` is public so the shell
crate's timeout test keeps driving it from outside the runtime crate.

## Alternatives considered

A new host-only process crate: rejected, since `p1-module-runtime` already owns the
capability traits the service implements. Moving `FinishOutcome` into `p1-finish-guest`:
rejected, since that crate is pure guest computation shared with the component, and the
cell is host state.

## Evidence

`cargo tree --locked -p p1-host -e normal | grep -E 'p1-tool-(shell|finish) '` prints
nothing, and `scripts/check-module-boundaries.sh --shipping` no longer lists either crate as a
native fallback. Tests: `cargo test --locked -p p1-tool-shell -p p1-tool-finish -p p1-shell-guest
-p p1-finish-guest`, the p1-host `activity` cases, `tests/activation.rs`, `tests/sandbox.rs`
and the p1-module-tests `shell_boundary.rs`/`finish_boundary.rs` pairs that drive the component
against the native tool.
