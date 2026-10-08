---
adr: 107
title: p1 builds and tests locally; GitHub Actions keeps only the required gate
status: superseded
date: 2026-09-30
deciders: owner+lead
supersedes: [66]
superseded_by: [128]
sources: [AGENTS.md, scripts/adr.py, docs/adr/0105-the-gate-runs-on-github-hosted-runners-only-the-stream-boxes-are-retired.md, .github/workflows/ci.yml, scripts/gate.sh, scripts/pre-push.sh, scripts/review-pr.sh, "PR #487"]
---
# ADR-0107: p1 builds and tests locally; GitHub Actions keeps only the required gate

## Context

ADR-0066 made GitHub Actions p1's build farm: every `task/**` push ran `scripts/gate.sh`
and a debug build in `.github/workflows/build.yml`, `scripts/ci-build.sh` waited for that
run, and local cargo was limited to `cargo check`. On a pull request that meant two full
gate runs per push (the required `gate` check in `ci.yml` and the `build` workflow), and
every defect cost a full run to find: `cargo test` stopped at the first failing test binary
and clippy at the first failing crate. PR #487 needed six such rounds (13–25 minutes each).

The 25 pull requests merged before 2026-09-30 (#382–#483) took a median of 180 minutes from
open to merged and a median of four gate runs; a gate run was in progress for 23 % of that
time (GitHub's run records). The time went into rounds: review findings, repairs and the
full gate each round restarts.

The owner, 2026-09-30: chose "Targeted local tests" in the question dialog, then: "We don't
need the build farm anymore. We will build locally." and asked for the fastest workflow from
pull request opened to merged without dropping reviews. On reviews, the same day: Codex
reviews are permitted, run locally with clear instructions on what to look for, so the lead
steers what is reviewed; the lead may decide not to review again when it or another agent
already reviewed the change, or when the change is too small to need one: "Wen cannot spent
hours in a fix / review loop for every PR!!!"

## Decision

- Before a push, `scripts/pre-push.sh` runs the parts of the gate a change can break: fmt,
  the workspace clippy with warnings denied, the modules, `cargo test --no-fail-fast` for the
  packages whose files changed, and the script tests when scripts, workflows, ADRs or
  `AGENTS.md` changed. Every step runs even after one fails, so one run reports every defect.
  Local targets follow the existing SSD target and rustc-slot rules.
- GitHub Actions keeps only the required `gate` check (`ci.yml`) on pull requests and main,
  and the release workflow. The `build` workflow, `scripts/ci-build.sh` and its tests are
  removed. The full gate stays GitHub's, as ADR-0105 decides; the pre-push checks are a
  subset of it and no evidence for a merge. ADR-0105's decision stands; three of its
  sentences change: "ADR-0066 stands unchanged" (ADR-0066 is superseded here), the `task/**`
  branch build in its Decision (removed here), and "No machine of ours verifies a change before
  its push" (the pre-push checks now do, for the packages a change touches).
- The gate reports every failure of a run: `cargo test --no-fail-fast` in `gate.sh` and in
  every test job, `cargo clippy --keep-going`. Its jobs build with four jobs, the runner's
  four processors (`CARGO_BUILD_JOBS`).
- Review: the lead decides per pull request (the levels are skill `pr-pipeline`'s). At most one
  review round, run locally with
  `scripts/review-pr.sh <pr> <focus-file>` (read-only `codex exec`), whose focus file names what
  to check and what to leave; none when the diff is
  small or already reviewed by the lead or another agent. One repair round takes the gate's
  failures and the confirmed P0/P1 findings together; nothing is reviewed again; lesser
  findings go into one follow-up issue. Auto-merge is set once no P0/P1 is open, so the merge
  follows the green gate without a wait for the lead.
- ADR numbers are unique, not dense: `scripts/adr.py check` still refuses a duplicate number but
  no longer a gap. Each change reserves its number on the board before `adr.py new`, and ADR
  pull requests merge in any order; under the dense rule a pull request could not merge before
  every lower number had, and a clash cost its second holder one more push and gate run.

## Consequences

- A PR runs one gate on GitHub instead of two, and most defects are found locally in one run.
- A red gate lists all its failures, so one repair round can take them all.
- Local test builds use the machine's RAM, SSD and rustc slots; they follow the build limits in
  AGENTS.md and the global rules. Until module compilation gets faster, `p1-host`'s tests are
  the slow part of a local run.
- There is no downloadable CI binary per task branch any more; a deployed binary is built
  locally, as the lead's deployed-binary rebuild already was.
- The owner-order text that says "no local tests" is superseded for p1 by the owner's order of
  2026-09-30; the global order file is updated by its maintainer.

## Alternatives considered

- Keep dense numbering and merge ADR pull requests in number order: on 2026-09-30 numbers 0105
  and 0106 were each carried by two open pull requests, and each clash cost one more push.

- Keep the build farm and add local tests: two GitHub gate runs per push remain for no gain.
- Keep CI only: one full run per defect, which is what this decision removes.
- Run the whole `scripts/gate.sh` locally before each push: it repeats GitHub's gate and holds
  the machine's build slots for the tests of crates the change does not touch.

## Evidence

PR #487: runs 36634342038, 36635152216, 36635881632, 36637696518 each surfaced one failure.
Gate run 36638989705: the workspace test job took 24 minutes, 1191 s of it test execution,
1109 s of that `p1-host`. The `build` workflow run on a task branch took 22–25 minutes
(run 36640244164: gate step 1441 s). The `build` workflow was disabled on 2026-09-30
(`gh workflow disable build.yml`) before this change removed it.
