---
adr: 96
title: Bubblewrap credential masks follow workspace mounts
status: proposed
date: 2026-09-27
deciders: lead
supersedes: [35]
superseded_by: []
sources: []
---
# ADR-0096: Bubblewrap credential masks follow workspace mounts

## Context

ADR-0035 makes bubblewrap mount order part of the contract. Issue #429 requires workspace and
writable binds before credential masks, but the accepted design text still specified the old
order and declared `bwrap_args` infallible. A later bind can cover an earlier credential mask.

## Decision

Place credential masks after readable, writable and workspace binds. Make `bwrap_args` return
`Result<Vec<OsString>, SandboxError>` so unresolved or credential-exposing readable paths fail
closed. This supersedes ADR-0035's mount-order contract only; its sandbox boundary remains.

## Consequences

No later configured mount can expose a masked cargo credential. Callers must handle argument
construction errors; the design contract now matches runtime behavior.

## Alternatives considered

Keep masks before workspace binds: rejected because the workspace mount could cover them. Keep an
infallible argument builder: rejected because unsafe readable paths must be refused, not skipped.

## Evidence

Issue #429 and PR #454 review findings; regression tests in
`crates/p1-module-runtime/src/process/sandbox.rs`; updated contract in `docs/design/tools.md`.
