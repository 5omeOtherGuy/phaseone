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
- (2b) Core acceptance tests by two independent authors (glm53, sol) → then deepseek implements
  the core against the frozen suites (brief not written yet; spec = docs/design/core.md).
- (3a) deepseek: `p1-workspace` + read/edit/write tools (worktree `../phaseone-file-tools`).
- Lead: provider seam spec (`docs/design/providers.md`): shared http/SSE/retry helper,
  scripted transport, ONE conformance suite, Anthropic adapter first.

## Next
- (3b) search, shell tools; Anthropic adapter; headless host + assembly. Live smoke (lead, P1_LIVE=1).
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
| 09-20 00:3x | glm53/high | core acceptance tests | `phaseone-briefs/core-tests-glm53.out` | running |
| 09-20 00:3x | sol/medium | core acceptance tests | `phaseone-briefs/core-tests-sol.out` | running |
| 09-20 00:4x | deepseek/high | workspace + read/edit/write | `phaseone-briefs/file-tools.out` | running |
