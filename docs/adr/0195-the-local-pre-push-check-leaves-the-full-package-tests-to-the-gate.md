---
adr: 195
title: The local pre-push check leaves the full package tests to the gate
status: accepted
date: 2026-10-11
deciders: owner
supersedes: []
superseded_by: []
sources: [ADR-0107, ADR-0128, ADR-0169, scripts/pre-push.sh, PR #858]
---
# ADR-0195: The local pre-push check leaves the full package tests to the gate

## Context

`scripts/pre-push.sh` (ADR-0107) ran `cargo test` for every touched package and every package
that reads changed shipped data before each push. For a change that touches `p1-host` or
`p1-module-tests` that took close to an hour on the shared workstation (3612 s and 3392 s for
#129; #836's run for PR #858 was stopped at a 90-minute limit with no failure), longer than the
gate on GitHub (21 to 41 minutes), which runs the same tests again. Several sessions run it at
once and share the machine's three compile slots. On 2026-10-11 the owner agreed to the
dispatcher's proposal: locally fmt, clippy and the focused tests of the changed code; the full
tests of touched packages left to the gate.

## Decision

`scripts/pre-push.sh` runs fmt, clippy when Rust changed (`P1_PREPUSH_CLIPPY=0` skips it) and the
script tests when `scripts/`, `.github/`, `docs/adr/` or `AGENTS.md` changed. It names the
packages whose full tests the gate will run and runs them only with `P1_PREPUSH_TESTS=1`. Before
it, the author runs the focused tests of the changed code (`cargo test -p <crate> <name>`, the
test binaries that pin the changed behaviour).

## Consequences

A push no longer waits up to an hour for tests the gate repeats. A failure in an unfocused test
of a touched package now shows up in the gate, one gate run later, instead of locally; the
focused-test rule keeps the likely ones local. Clippy, which the gate also runs, is now on by
default, because a clippy failure costs a full gate run and clippy is fast next to the tests.
`P1_PREPUSH_TESTS=1` keeps the old behaviour for a change whose focused tests cannot be named.

## Alternatives considered

- Keep the full package tests and raise the build slots: the machine has 8 threads and 11 GiB
  shared by every session; more slots trade time for memory pressure.
- Drop the local check entirely: fmt, clippy and the script tests are fast and catch the
  failures that otherwise cost a whole gate run.

## Evidence

Step times are printed by every run (`== pre-push: summary`). `python3 scripts/test_pre_push.py`
pins that the default run selects but does not build or test the packages and that
`P1_PREPUSH_TESTS=1` tests them.
