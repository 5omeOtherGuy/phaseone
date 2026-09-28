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
`Result<Vec<OsString>, SandboxError>`. One bind-source check at assembly and command start rejects lexical and canonical credential exposure for workspace, readable, writable and home-visible entries. Writable ancestors of hidden home, workspace and private tmp are refused. Canonical sources already under writable roots (including missing descendants) are not rebound; aliases are restored with `--symlink` so hidden-home aliases stay reachable. Duplicate writable binds are redundant. An executable-only unsafe PATH fails assembly with `UnsafeLauncher`; a cached launcher that becomes unsafe fails command start as `ProcessFailure::Start`. This supersedes ADR-0035's mount-order contract only; its sandbox boundary remains.

## Consequences

No later configured mount can expose a masked cargo credential or rebind mutable sources through aliases. Callers handle argument-construction errors; redundant entries retain configured alias paths without mutable rebinds.

## Alternatives considered

Keep masks before workspace binds: rejected because the workspace mount could cover them. Keep an
infallible argument builder: rejected because unsafe readable paths must be refused, not skipped.

## Evidence

Issue #429 and PR #454 review findings; regression tests in
`crates/p1-module-runtime/src/process/sandbox.rs`; updated contract in `docs/design/tools.md`.
