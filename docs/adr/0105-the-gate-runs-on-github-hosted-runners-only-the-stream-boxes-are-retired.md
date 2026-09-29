---
adr: 105
title: The gate runs on GitHub-hosted runners only; the stream boxes are retired
status: proposed
date: 2026-09-30
deciders: owner
supersedes: [77]
superseded_by: []
sources: [AGENTS.md, docs/adr/0066-p1-builds-and-verifies-on-github-actions-local-cargo-only-for-check-and-the-lead-s-deployed-binary.md, docs/adr/0077-builds-on-the-stream-boxes.md, docs/adr/0097-github-hosted-ci-provisions-bubblewrap-and-requires-the-sandbox-suites.md, .github/workflows/ci.yml, .github/workflows/build.yml]
---
# ADR-0105: The gate runs on GitHub-hosted runners only; the stream boxes are retired

## Context

ADR-0077 put cargo builds, tests and the full `scripts/gate.sh` on each stream's EC2 box and
made GitHub Actions the merge gate only. Its reason for the box gate was that the GitHub-hosted
variant skipped the bubblewrap suites. ADR-0097 removed that reason: GitHub-hosted runners
provision bubblewrap and a skipped sandbox suite fails the job. ADR-0097 still kept ADR-0077's
build placement, so every change went through two gate runs: one on a box before the push, one
in the pull request's required `gate` check.

The fix campaign after the WebAssembly cutover (pull requests #463 to #483, 2026-09-28 and
2026-09-29) worked under that rule. Its log counts 109 gate submissions to the boxes for 11
pull requests, 44 of them red, and the required check then ran the same script again on every
push. The campaign's lead named the box gate as a repeated check in its lessons report.

The owner decided on 2026-09-29, on the review of that report: the build boxes no longer verify
("F - agreed"), the rule lines that demand the second gate run are corrected ("D - Correct
them"), the gate stays on GitHub's runners and gets no second CI provider beside them ("GitHub
only"), and the three boxes are deleted once the campaign's lead has saved its logs.

## Decision

`scripts/gate.sh` runs on GitHub-hosted runners only: in the pull request's required `gate`
check and in main's `gate` run on the merge commit (`.github/workflows/ci.yml`), and in the
`task/**` branch build (`.github/workflows/build.yml`). No stream box and no workstation runs
the gate before a push, and a gate run outside GitHub Actions is no evidence for a merge. The
stream boxes are retired as build and verification machines.

One exception stays, and it is ADR-0066's: while Actions cannot be used, a full local gate may
be run as an announced extra step. It informs the author and never replaces the required
check, so nothing merges on it.

This supersedes ADR-0077's build placement. ADR-0097's decision stands; its sentences that keep
ADR-0077's build placement and the stream boxes' full gate no longer apply. ADR-0066 stands
unchanged: local cargo is `cargo check -p <crate>` plus the lead's deployed-binary rebuild.

## Consequences

- No machine of ours verifies a change before its push; the first result arrives after it,
  from the run whose result merges the change. A `task/**` push with an open pull request
  still starts two runs of the same script on GitHub (`build.yml` for the branch, `ci.yml` for
  the pull request); this decision does not change that.
- A stream has no build and test loop of its own any more, which was ADR-0077's reason for
  the boxes. CI is a queue; the durations under Evidence are what a stream waits.
- While Actions cannot be used, nothing merges: main requires the `gate` check, and the local
  gate of the exception above informs but does not merge.
- The sandbox suites are proven on GitHub-hosted runners only (ADR-0097). A defect that shows
  only on another kernel or bubblewrap version is no longer seen on a second machine.
- Scripts and settings written for the boxes are not changed by this decision; removing them
  is separate work.

## Alternatives considered

- Keep the box gate before the push (ADR-0077): rejected by the owner. It runs the same script
  as the required check, so every change paid for two runs.
- A second CI provider beside GitHub (pull request #480, closed unmerged): rejected by the
  owner. The same checks would run twice on every push, GitHub's run stays the one that counts
  for the merge, and the repository is public, so GitHub's standard runners cost nothing.
- Keep the boxes stopped for runs CI cannot do: not chosen; the owner chose deletion.

## Evidence

- Branch protection of main: required check `gate`, admins included
  (`gh api repos/5omeOtherGuy/phaseone/branches/main/protection`, read 2026-09-29).
- Of the 40 most recent `gate` runs on 2026-09-29, 32 were green and took 11.8 to 30.4 minutes
  (median 18.2), 3 were red and ended after 4.5 to 6.2 minutes, 5 were cancelled. Recomputed
  from GitHub's run records, creation to last update (`gh run list --workflow gate`).
- Box gate counts (109 submissions, 44 red, 11 pull requests): counted by script over the
  campaign's log on the owner's workstation
  (`~/.agents/xo/dispatch/cutover-lead/fix3/pipeline.log`); the log is not in this repository.
- Pull request #480 and issue #479 were closed on 2026-09-29 on the owner's decision.
- The three boxes were stopped when the decision was taken; their deletion is with the
  campaign's lead.
