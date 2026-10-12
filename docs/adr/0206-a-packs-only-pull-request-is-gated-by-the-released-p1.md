---
adr: 206
title: A packs-only pull request is gated by the released p1
status: accepted
date: 2026-10-12
deciders: owner
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/854, https://github.com/5omeOtherGuy/phaseone/issues/840, docs/adr/0200-phaseone-s-core-is-the-bare-product-subscription-connectors-and-the-owner-pack-are-optional-parts.md, docs/adr/0128-fast-lane-one-worker-per-issue-review-before-the-push-a-docs-only-gate-path-and-ssd-only-builds.md, docs/adr/0192-a-pull-request-reuses-an-earlier-green-gate-when-its-code-is-a-clean-merge-of-that-head-with-main.md]
---
# ADR-0206: A packs-only pull request is gated by the released p1

## Context

The repository split (epic #840, ADR-0200) moved the owner's environments, accounts, scripts and
records into the owner pack, `packs/owner/` (#853). A pack is data and process scripts, not Rust:
a change to it cannot change what the workspace compiles or how its tests behave, yet every such
pull request ran the full gate (checks, five test shards and the bare build), the slowest path
CI has. The owner's plan for the split (#854): a pull request that changes only `packs/` runs
`p1 env check` with the released p1 and the pack's own tests, in `ci.yml`, with no new workflow.

## Decision

`ci.yml`'s `classify` job marks a change `pack_only` when every changed file, and every renamed
file's old name, is under `packs/`. On that path the Rust jobs are skipped, `docs` (ADR check and
secret scan) runs as on the other fast paths, and a `pack` job installs the latest release with
`scripts/install.sh --from-release` and runs `p1 env check --dir packs/owner`, the pack's tests
(`packs/owner/tests/`) and its script tests (`packs/owner/scripts/test_*.py`) against it. The
required `gate` job accepts the path only when `classify`, `docs` and `pack` all succeeded.

## Consequences

- A pack change is gated in minutes, by the p1 the owner actually runs: the latest release is
  main's head, so the pack is checked against the code it ships with.
- A pack change that needs a p1 change is not pack-only: it touches files outside `packs/` and
  runs the full gate, which is the case where the release would be the wrong binary to test.
- The prompt sweep that renders every tool subset of the owner's prompts lives in p1-assembly
  (#853), so a pack-only prompt edit skips it; `test_pack.py` still renders each prompt with
  its full tool set and holds the placeholder rule. The next full gate (any Rust change, and
  every push to main that is not a fast path) runs the sweep again.
- `scripts/test_gate_reuse.py` runs the `classify` step as written against fixed file lists, and
  `scripts/test_gate.py` runs the `gate` job's check over every combination of job results.

## Alternatives considered

- A separate workflow for `packs/`: rejected by the owner's plan ("no new workflow file"), and a
  second workflow would need its own required check.
- Building p1 from the pull request on the pack path: correct for a change that also needs code,
  but that change is not pack-only; for a pack-only change the build only repeats main's gate.
- Treating the pack as documentation (the docs-only path): skips `p1 env check`, so a broken
  environment file would land unchecked.

## Evidence

`python3 scripts/test_gate_reuse.py` (the `classify` cases: pack files only, a file outside,
a rename out of the core, pack Markdown, an unreadable or oversized change) and
`python3 scripts/test_gate.py` (the required job on each path); `scripts/test_ci_bwrap.py` pins
the `pack` job's steps. The first pull request that changes only `packs/` shows the path on CI.
