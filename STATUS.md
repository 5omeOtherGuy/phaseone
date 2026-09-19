# Status

Handoff: `/home/phaseonebig/projects/phaseone-collab/fable-orchestrator-prompt.md`.
After any context compaction: re-read this file and `DECISIONS.md` first.
Remote: github.com/5omeOtherGuy/phaseone (PUBLIC). Trunk-based: `task/*` branches, merge to
`main` when the gate is green, push (D6, D11). Shared target dir, one compile at a time (D12).
Only the lead edits this file (D13). Started 2026-09-19.

## Done (all on main, gate green, pushed, CI green)
- Build/infra: gate, core-isolation check, per-worktree seeded targets + global rustc semaphore
  (D20, replaces the unsound shared target of D12), direct worker fan-out `scripts/fanout.py`.
- Specs in `docs/design/`: routes (+ live results §D), core (R1-R6), tools, providers (+ origin
  ruling), assembly, journal, delegation.
- Crates (19): contracts, testkit, core (+ project/resume), workspace, tool-read/edit/write/
  search/shell/patch, tool-tests (lead adversarial), provider-http, provider-conformance,
  provider-anthropic, provider-openai, journal, assembly, workers, tool-delegate, live (lead).
- BOTH adapters pass the ONE conformance suite (15 checks) and the LIVE smoke checks
  (2026-09-20): text, tool call, tool-result follow-up; Codex route accepts freeform apply_patch.
- `environments/{claude,gpt}`: config + family prompts, coherence-tested.

## In progress
- nothing. The first slice is DONE (2026-09-20): every seams.md §10 acceptance item is
  demonstrated by a command in `docs/SLICE-REPORT.md`, including live runs on both routes,
  live cross-route delegation and live resume.

## Next (owner's call — see `docs/SLICE-REPORT.md` "What should come next")
- Use it for real work and collect failures; context-control policy module; T1 comparison;
  granular authorization; turn-completion policy.

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
| 09-20 | deepseek/high | search + shell + patch tools | see evidence.jsonl | ACCEPTED, lead fixed 2, $0.16 |
| 09-20 | sol/medium | conformance suite | `.worker-runs/20260920-003643-1491887` | ACCEPTED, $1.44 |
| 09-20 | deepseek/high | anthropic adapter | `.worker-runs/20260920-003643-1491888` | ACCEPTED (lead brief error fixed), $0.12 |
| 09-20 | deepseek/high | journal + resume | see evidence.jsonl | ACCEPTED first pass, $0.08 |
| 09-20 | deepseek/high | assembly | see evidence.jsonl | ACCEPTED first pass, $0.07 |
| 09-20 | deepseek/high | delegation | see evidence.jsonl | ACCEPTED first pass, $0.12 |
| 09-20 | deepseek/high | codex adapter | see evidence.jsonl | ACCEPTED (same brief error fixed), $0.11 |
| 09-20 | deepseek/high | host | `.worker-runs/20260920-010944-1597843` | ACCEPTED, lead fixed 3 from live runs, $0.32 |
