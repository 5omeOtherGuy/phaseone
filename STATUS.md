# Status

Handoff: `/home/phaseonebig/projects/phaseone-collab/fable-orchestrator-prompt.md`.
After any context compaction: re-read this file and `DECISIONS.md` first.
Remote: github.com/5omeOtherGuy/phaseone (PUBLIC). Trunk-based: `task/*` branches, merge to
`main` when the gate is green, push (D6, D11). Shared target dir, one compile at a time (D12).
Only the lead edits this file (D13). Started 2026-09-19.

## Done
- (0) Repo, workspace, `AGENTS.md`, gate, core-isolation check, shared-target build policy
  (D12), direct worker fan-out `scripts/fanout.py` (D14/D15). GitHub repo + CI by a separate session.
- (1a) Route shapes from the donor: `docs/design/routes.md` ([todo-live] items remain).
- (2a) `p1-contracts`, `p1-core` API stub (`unimplemented!`), `p1-testkit`, spec `docs/design/core.md`.
- Tool spec `docs/design/tools.md`.

## In progress
- (2b) Core: frozen suites `acceptance_sol.rs` (37, accepted) + `acceptance_lead.rs` (7) are
  committed on branch `task/core-impl` (worktree `../phaseone-core-impl`), NOT on main (D19).
  deepseek implements against them (brief `phaseone-briefs/core-impl.md`). glm53's suite, when
  it lands in `../phaseone-core-tests-glm53`, is a HELD-OUT check: run it against the finished
  core, adjudicate each failure against docs/design/core.md + rulings R1-R4.
- (3a) deepseek: `p1-workspace` + read/edit/write tools (worktree `../phaseone-file-tools`).
- (3c) deepseek: `p1-provider-http` (worktree `../phaseone-provider-http`).
- All seam specs are written: routes, core, tools, providers, assembly, journal, delegation.
  Environment files + family prompts are in `environments/`.

## Next
- After file-tools lands: brief search + shell tools (need `p1-workspace`), then apply_patch.
- After provider-http lands: conformance suite by an independent author (sol or glm53) against
  its API, then the Anthropic adapter (deepseek), then the Codex adapter.
- After core lands: `p1-journal` (+ `project`/`resume` in core), `p1-assembly`, `p1-host`.
- Live smoke (lead only, P1_LIVE=1): first the [todo-live] items in routes.md.
- (4) Codex adapter + apply_patch tool + GPT environment; assembly fails fast on bad combos.
- (5) JSONL journal, resume, interrupted-call reconciliation.
- (6) Optional delegation tool + in-process worker service.
- (7) Slice acceptance (seams.md §10), measurements, `docs/SLICE-REPORT.md`.

## Blocked
- nothing

## How to resume
Briefs + job lists + worker outputs: `/home/phaseonebig/projects/phaseone-briefs/`.
Worktrees: `git worktree list`. Dispatch: `scripts/fanout.py <jobs.json>` as ONE background task.
Accept a result: rebuild + rerun tests yourself, adversarial cases, read diff, `git status`,
commit explicit paths on the task branch, merge to main, gate, push, log evidence to
`~/.agents/skills/model-cards/evidence.jsonl`, remove the worktree.

## Worker runs
| When | Profile/effort | Task | Run dir / out file | Result |
|---|---|---|---|---|
| 09-20 00:04 | deepseek/high | fan-out mechanism trial | `.worker-runs/20260920-000458-1394036` | ok, $0.0006 |
| 09-20 00:14 | glm53/high | core acceptance tests | `.worker-runs/20260920-001409-1410498` | running |
| 09-20 00:14 | sol/medium | core acceptance tests | `.worker-runs/20260920-001409-1410499` | ACCEPTED, 37 tests, 357 s, $0.86, evidence logged |
| 09-20 00:16 | deepseek/high | workspace + read/edit/write | `phaseone-briefs/file-tools.out` | running |
| 09-20 00:20 | deepseek/high | p1-provider-http | `phaseone-briefs/provider-http.out` | running |
| 09-20 00:24 | deepseek/high | core implementation | `phaseone-briefs/core-impl.out` | running |
