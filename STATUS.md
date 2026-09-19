# Status

Handoff: `/home/phaseonebig/projects/phaseone-collab/fable-orchestrator-prompt.md`.
After any context compaction: re-read this file and `DECISIONS.md` first.
Remote: github.com/5omeOtherGuy/phaseone (PUBLIC). Trunk-based: `task/*` branches, merge to
`main` when the gate is green, push (D6, D11). Shared target dir, one compile at a time (D12).
Only the lead edits this file (D13). Started 2026-09-19.

## Done (all on main, gate green, pushed)
- (0) Repo, gate, core-isolation check, shared-target build policy (D12), direct worker fan-out
  `scripts/fanout.py` with machine-wide pool bounds (D14/D15). GitHub repo + CI by another session.
- (1a) Route shapes from the donor: `docs/design/routes.md` ([todo-live] items remain).
- All seam specs: routes, core (+ rulings R1-R6), tools, providers, assembly, journal, delegation.
- (2) `p1-contracts`, `p1-testkit`, `p1-core` — 101 tests in four independently written suites
  (sol 37, glm53 42 held-out, lead 10, implementer 12).
- (3a) `p1-workspace`, `p1-tool-read`, `p1-tool-edit`, `p1-tool-write`, lead cross-tool tests
  `crates/p1-tool-tests`.
- (3c) `p1-provider-http` (transport seam, SSE, retry driver, ScriptedTransport).
- `environments/{claude,gpt}` config + family prompts.

## In progress (worktrees `../phaseone-<slug>`, briefs in `../phaseone-briefs/<slug>.md`)
- `more-tools` deepseek: p1-tool-search, p1-tool-shell, p1-tool-patch.
- `conformance` sol: p1-provider-conformance with seeded-bug self-test.
- `anthropic` deepseek: p1-provider-anthropic (+ ClaudeCodeCredentials). After it lands the
  LEAD adds its `conformance()` test and runs the first live smoke (P1_LIVE=1).
- `journal` deepseek: p1-journal + `project`/`Agent::resume` in core. Lead adds adversarial
  truncation/resume tests on acceptance.
- `assembly` deepseek: p1-assembly.

## Next
- `p1-provider-openai` (Codex route) after conformance + anthropic land (same brief shape;
  first live check decides freeform apply_patch vs function face — routes.md [todo-live]).
- `p1-host` (binary `p1`): composition root, headless + prompt loop, authorization policy,
  usage line. Needs assembly + tools + one provider.
- (6) p1-workers + p1-tool-delegate (spec: docs/design/delegation.md), cargo feature in host.
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
| 09-20 00:14 | glm53/high | core acceptance tests (held-out) | `.worker-runs/20260920-001409-1410498` | ACCEPTED after 1 repair (10 false alarms), $1.02 |
| 09-20 00:14 | sol/medium | core acceptance tests | `.worker-runs/20260920-001409-1410499` | ACCEPTED, 37 tests, $0.86 |
| 09-20 00:16 | deepseek/high | workspace + read/edit/write | `.worker-runs/20260920-001631-1420700` | ACCEPTED, lead fixed 1 defect, $0.10 |
| 09-20 00:20 | deepseek/high | p1-provider-http | `.worker-runs/20260920-001941-1429195` | ACCEPTED, lead fixed 2 defects, $0.08 |
| 09-20 00:24 | deepseek/high | core implementation + R5 | `.worker-runs/20260920-002352-1451905` | ACCEPTED first pass, $0.11 |
| 09-20 00:3x | deepseek/high | search + shell + patch tools | `phaseone-briefs/more-tools.out` | running |
| 09-20 00:4x | sol/medium | conformance suite | `phaseone-briefs/conformance.out` | running |
| 09-20 00:4x | deepseek/high | anthropic adapter | `phaseone-briefs/anthropic.out` | running |
| 09-20 00:5x | deepseek/high | journal + resume | `phaseone-briefs/journal.out` | queued/running |
| 09-20 00:5x | deepseek/high | assembly | `phaseone-briefs/assembly.out` | queued/running |
