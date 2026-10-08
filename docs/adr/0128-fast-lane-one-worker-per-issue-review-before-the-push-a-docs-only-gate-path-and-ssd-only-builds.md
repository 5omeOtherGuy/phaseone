---
adr: 128
title: Fast lane, one worker per issue, review before the push, a docs-only gate path and SSD-only builds
status: accepted
date: 2026-10-08
deciders: owner
supersedes: [107, 119]
superseded_by: []
sources: [AGENTS.md, .github/workflows/ci.yml, scripts/pre-push.sh, scripts/retire-worktree.sh]
---
# ADR-0128: Fast lane, one worker per issue, review before the push, a docs-only gate path and SSD-only builds

## Context
Between 2026-09-20 and 2026-09-26 main took 40 to 260 commits a day; since 2026-09-27 it takes
3 to 25. The rules that arrived in that week each added a serial wait to every change: a design
record before each slice, a review round after the push with its own repair, pre-push and gate
run, a 16-section brief, one issue at a time, and bookkeeping pull requests (11 of the last 100
only flipped a record's status or the status file) that each paid the full 15-minute gate. The
cheapest worker model took 2.7 to 5.8 hours per Rust slice with repairs where Opus took 25 to 40
minutes, and the slices the lead implemented itself landed in about an hour against three to
eight through a worker. Targets moved to the data-tier HDD after stale worktrees filled the SSD.
The owner decided on 2026-10-08 to cut the paperwork, keep the review but move it before the
first build, use Opus workers that own an issue end to end, build on the SSD only, and adopt
rules as light as iris-agent's.

## Decision
1. One worker owns one issue end to end: worktree, code, tests, self-review, pre-push, pull
   request, auto-merge, retirement. Opus at medium or high effort by default. The lead dispatches
   and verifies the merge; up to three issues run at once on disjoint files.
2. The review happens before the first push: the worker runs one read-only reviewer on its
   diff, fixes P0 and P1 findings, then runs `scripts/pre-push.sh` once and pushes once. P2 and
   P3 findings go into the pull request body. After the push the gate is the only check; there is
   no review round and no repair round.
3. Small changes accumulate into the next pull request on the same area; a status or record
   change never gets its own pull request when a code pull request is due the same day.
4. A docs-only change (every changed file is Markdown) takes the gate's fast path in
   `.github/workflows/ci.yml`: the Rust jobs are skipped and the `light` job (ADR check, script
   tests) is the whole gate. Any other file runs the full gate.
5. A design record is written only for a changed public interface or contract, a dependency or
   workflow rule, or a reversed decision, and it lands `accepted` in the pull request that lands
   its code. No record before a slice; no separate accept pull request.
6. `scripts/pre-push.sh` runs once per pull request: fmt, the modules when touched, the tests of
   the touched packages, the script tests when scripts, CI, records or `AGENTS.md` changed. Clippy
   runs in CI; `P1_PREPUSH_CLIPPY=1` adds it locally. The gate script itself is unchanged.
7. Everything builds on the SSD: targets below `~/.cache/cargo-target/<task>`, no `/data/build`
   targets. `scripts/retire-worktree.sh <path>` removes a worktree, its branch and its target as
   soon as its pull request merged; it refuses a dirty tree or an unmerged branch.
8. Briefs are at most ten lines: issue, owned paths, definition of done with its commands, what
   not to touch. No evidence rows, stage tables or decision-log files for p1 work.
9. Owner questions go as one batched entry to `~/.agents/xo/for-owner.md`; the worker or lead
   proceeds on the least risky option labelled as an assumption.
ADR-0107's placement of the full gate on GitHub only stands; its mandated local clippy and its
one-review-round-after-push landing rule are replaced by points 2 and 6. ADR-0119's per-task
targets and three-build limit stand; its data-tier overflow is withdrawn by point 7.

## Consequences
Fewer serial waits per issue and no gate run for a status flip. Some P1 defects that a post-push
review caught (four of the last ten pull requests) will reach main and be fixed afterwards; the
owner accepts that trade. Worktree hygiene is enforced by the retire script instead of by disk
pressure. `scripts/gate.sh`, branch protection and the required `gate` check are unchanged.

## Alternatives considered
Skipping the gate for docs-only pull requests: impossible, the check is required by branch
protection, so the fast path lives inside the workflow instead. Keeping the post-push review and
parallelising only: rejected, each review round still costs a repair, a pre-push and a gate run.
Dropping the review altogether: rejected by the owner; the review moves before the push instead.

## Evidence
Measured 2026-10-08 from git, the GitHub API, the lead's notes and the pre-push logs: merged
pull requests per day, CI run durations (12 to 16 minutes), pre-push step times (clippy average
143 s, maximum 612 s; modules average 76 s, maximum 635 s), worker elapsed times in
`~/.agents/skills/model-cards/evidence.jsonl`. Owner decision: session of 2026-10-08, all seven
cuts agreed.
