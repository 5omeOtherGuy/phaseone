---
adr: 194
title: The gate schedules every compiled test target with bounded concurrency and refreshes isolated caches
status: accepted
date: 2026-10-11
deciders: owner+lead
supersedes: []
superseded_by: []
sources: ["Issue #839", scripts/gate.sh, scripts/ci-tests.py, scripts/build-modules.sh, .github/workflows/ci.yml, docs/testing-and-ci-audit.md]
---
# ADR-0194: The gate schedules every compiled test target with bounded concurrency and refreshes isolated caches

## Context

The owner asks for a complete tests/CI audit and a gate of ten minutes or less.
Two full successful gates measured 24m 29s and 28m 09s on 2026-10-11. The
workspace job ran every test binary serially; exhaustive prompt rendering and
composed-provider conformance dominated its execution. Setup was copied across
four jobs, and lockfile-only immutable caches never refreshed after their first save.

## Decision

Keep every existing gate obligation and frozen acceptance case. CI invokes
`gate.sh --checks` explicitly for non-workspace obligations; this phase cannot print
GREEN. Remove the PATH cargo wrapper that returned success for deferred tests.
Two required partitions cover every native workspace package: p1-module-tests,
and all other packages, including newly added ones.

Cargo discovers and builds every default test target on each test runner. A stdlib
scheduler verifies the inventory and assigns the workspace targets to four required
shards using committed measured costs. New targets get a default cost. Cached
timings only order execution; they cannot change shard membership. Workspace shards
run up to two complete libtest binaries at a time; the module partition runs four,
retaining per-process component reuse, package working directories and Cargo package
metadata. Cargo runs each partition's doctests once, in shard 1. Workspace shard 1
also runs the host's existing feature-off acceptance. Any failed target, missing
artifact, timeout or sandbox skip is red; matrix fail-fast is disabled.

Shared composite actions own pinned tools, sandbox provisioning and cache paths.
Caches stay partition-specific and refresh on successful main commits using
commit-qualified keys. PRs never save. Workspace shard 1 saves its complete compilation
cache; peers do not race to save the same key. CI disables incremental compilation and
omits incremental/candidate scratch from saved artifacts. The guest build uses
one Cargo graph invocation after validating all selected manifests. Its component
validation and atomic publication stay unchanged. Local pre-push requests build
admission only for steps that actually compile. Optimize development/test prompt
rendering and host-side component instantiation. Reuse a paused conformance executor's
blocking pool across a fixture's byte splits, while preserving fresh providers and
requests for every split and separate runtimes for timing-specific checks. Synchronize
the shared-workspace acceptance on both queued writers, retaining all its assertions.

## Consequences

Independent suites can overlap without changing their assertions or filtering
individual tests. New packages, integration targets and Python suites become
required automatically. The checks phase and test matrix must all succeed before
the aggregate gate is green. Default gate.sh retains its complete serial contract.

Concurrency can expose global-resource assumptions in tests; those must be fixed
with synchronization rather than retries or skipped coverage. Fresh per-main cache
entries may evict older entries. A cold-cache run or a queued runner can exceed ten
minutes; timings must report those costs instead of declaring success based on a
timeout or fast-path run. The durable audit records measurements and limitations.

## Alternatives considered

Dropping or sampling the exhaustive sweeps: would weaken frozen acceptance coverage.
A separate process per individual test: discards the verified component cache that
these suites deliberately share within each libtest binary. Moving obligations to
the release workflow: would weaken the merge contract. Silently skipping tests in
a cargo PATH wrapper: replaced by an explicit phase and required aggregate result.

## Evidence

[Run 38146489661](https://github.com/5omeOtherGuy/phaseone/actions/runs/38146489661)
and [run 38146774375](https://github.com/5omeOtherGuy/phaseone/actions/runs/38146774375)
provide the measured baseline. The optimized prompt acceptance suite passed all
five cases in 99.14 seconds locally, with concurrent workstation activity.
`scripts/test_ci_tests.py`, `scripts/test_gate.py`, `scripts/test_ci_bwrap.py` and
`scripts/test_build_modules.py` validate scheduling, coverage and failure behavior.
The complete inventory, before/after GitHub measurements and remaining findings
are in `docs/testing-and-ci-audit.md`.
