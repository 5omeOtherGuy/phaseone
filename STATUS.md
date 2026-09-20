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

- ADR system added after the slice (owner request): `docs/adr/` (30 ADRs), `scripts/adr.py`,
  checked in the gate. New decisions are ADRs; `DECISIONS.md` is the frozen ledger (ADR-0030).

## Next — READ FIRST after compaction
State 2026-09-20 evening: main = green gate + green CI on the exact commit (always push with
`scripts/push-main.sh`). ADRs 0031–0040. No lead worktree open; only Astra's
`../phaseone-9-subscription-routes`. Owner messages arrive mid-turn; Astra sits in tmux pane %39 (codex).

HOW THE LEAD WORKS NOW (owner instructions, also in memory):
- Implementation goes to deepseek workers by default (`scripts/fanout.py <jobs.json>`, briefs and
  outputs in `../phaseone-briefs/`); same-session repairs via `session` + `prompt_file`. Lead
  keeps: specs, ADRs, diff review, adversarial tests, live checks, merges. Typical job: $0.01–0.15.
- Full access is the DEFAULT in p1 (ADR-0038, `--ask` opts out). The auto-mode classifier
  BLOCKED dispatching a change that makes fanout's p1 runner unsandboxed by default — not worked
  around; the runner sandboxes always; owner may authorise explicitly.
- Goal (owner): as soon as the cheap routes exist, run development jobs THROUGH p1
  (`"runner": "p1"` in a fanout job) and watch the harness closely; every job is a dogfood run
  → record in `docs/dogfood/runs.jsonl` (`scripts/run-report.py`), group findings in issue #6.

DONE since the review (all merged): review R1–R7 + dispositions; write gate (#1); resume
decisions (#2/#3); shell sandbox (ADR-0035); context control (ADR-0036, live canary ok, shipped
envs still WITHOUT `[context]`); shell env allow-list (#4); read tool streams + long lines +
`file_path` (dogfood runs 1–3, all p1's own changes, all accepted, run 2 after a repair turn);
Codex cache headers (#7: 9–27 % → 57–69 %); turn completion = `finish` tool + bounded
continuation (ADR-0037) and its usability revision; worker usage files (#8); fanout p1 runner;
full-access default + cache key OFFERED not imposed.

OWNER DECISIONS 2026-09-20 (ADR-0039, ADR-0040; note `docs/design/notes/2026-09-20-provider-split.md`):
- Provider = wire adapter × route (data) × model profile (data + few compiled strategies, new
  crate `p1-model-profile`), ONE runtime `Provider`; environment names `route` + `profile`.
- RESHAPE FIRST: Astra reshapes `p1-provider-openai-chat` (note's step 2) BEFORE merging; told
  so on issue #9 — CHECK #9 between jobs, Astra only talks there.
- Credentials: `~/.config/p1/auth.json` keyed by route, env var wins, borrow other tools' logins
  for now, no login command yet, macOS later.

PROGRESS 2026-09-20 late (all merged, CI green on the exact commit):
- Astra's step 2 merged (511e7b4, #9 closed): `p1-provider-openai-chat` (ChatRoute, ChatDialect
  by behaviour, separate `p1-model-profile`), host `auth.rs`, envs `deepseek` and `glm`.
- DOGFOODING IS LIVE: lead jobs run as `"runner": "p1"` (env deepseek; glm quota ran out after
  one 21M-token job). Every run recorded in `docs/dogfood/runs.jsonl`.
- Split step 1 (characterization tests), 3a (profiles as files, env `route`+`profile`, spec
  `docs/design/routes-and-profiles.md`), 3c (RouteDescription.cache_key, one assembly, foreign
  native option = error, Anthropic cap conflict = error, empty OpenAI cache key = error).
- #11 / ADR-0041: headless runs wait and continue after Transport/RateLimited turn ends
  (`--provider-retries`). `deepseek` env has first measured `[context]` values — proven in real
  work (171 requests, 3 summarizations, accepted first round).
- Also merged since: 3b route files, `--sandbox-read`, summary-quality fix (#6: truncated
  summaries never accepted, `[context] summary_output_tokens`, `## Files` section), fanout p1
  runner = FULL ACCESS by default (`"sandbox": true` opts in; owner authorised). Step-4 spec =
  `docs/design/routes-and-profiles.md` §7. Owner switched Claude Code to bypassPermissions
  (no classifier any more): AGENTS.md rules are the only guard — work carefully.
- IN FLIGHT when this was written (2026-09-20 ~18:45):
  * split 4a (Anthropic policy → profiles/route file): p1 run, shell task `bem0itt96`,
    worktree `../phaseone-split4a-anthropic`, run dir `../phaseone-briefs/runs/split4a-20260920-174449/`
    (brief `../phaseone-briefs/split4a.md`). If the notification is lost: check
    `pgrep -af "debug/p1"`, then `report.json`/`stdout.txt` there; review diff, merge main,
    independent gate, merge, `scripts/push-main.sh`, record in `docs/dogfood/runs.jsonl`.
  * TUI: separate interactive Kimi K3 session (tmux window `kimi-tui`, pane %48, worktree
    `../phaseone-12-tui`), coordination ONLY via issue #12; its plan is accepted. LEAD OWES
    (promised on #12, do right after 4a merges): generalise `make_child_factory` to
    `Arc<dyn AuthorizationPolicy>` + per-child `EventSink` factory, and ONE front-end branch
    point in `run_agent` (`tui::run` vs `run_interactive`); later a small spec+ADR for
    `ContextStats` (observation-only context budget numbers). Lead never edits `crates/p1-tui*`
    or `docs/design/tui/`.

- 2026-09-20 ~21:00: the PRIMARY opencode-go subscription ran out of credit (401 CreditsError).
  Owner rule: use the second account, DeepSeek V4.1 Flash ONLY — `pi-worker deepseek2 …`, and in
  p1 `--env deepseek2` (route `opencode-go-2-subscription`, a data-only addition). p1 jobs need
  the key in the environment: dispatch with
  `OPENCODE_GO_2_API_KEY="$(tr -d '\n' < ~/.config/keys/opencode-go-2.key)" scripts/fanout.py …`
  (never print it). The z.ai GLM plan is small (5-hour quota) — use `--env glm` sparingly.
  Steps done since: 4a-1, 4a-2 (Anthropic from route file + profile), FrontEnd seam (#12),
  stall guard (ADR-0042), chat-parser tool-type fix. In flight: 4b (OpenAI Responses) as
  `split4b-glm` in `../phaseone-split4b-openai`; if it dies, re-dispatch the same brief
  (`../phaseone-briefs/split4b.md`) with env `deepseek2`.

NEXT, in order:
1. Review + merge 4a; then the host seam for the TUI (above); then 4b = the same for
   `p1-provider-openai` (brief not written yet; mirror `split4a.md`).
2. Split step 4 (spec §7): first-party policy extraction (Claude classification/budgets, GPT defaults →
   profiles; claude/gpt envs to `route`+`profile`; characterization tests must stay byte-equal).
   Step 5: `p1-auth` per ADR-0040 (borrow-only, write-back to the source).
3. `[context]` for glm/claude/gpt envs from runs (glm grew to 203k/request without it) — #6.
   Small harness debts from dogfooding (#6): a run with zero responses prints `in 0 … cost
   $0.0000` instead of unknown; run-report `elapsed_seconds` null when called by hand.
4. T1 measurement (fixed task set, repeated), host cleanup around the state transitions, model
   cards from `~/.agents/skills/model-cards/evidence.jsonl`.
Known small debts: `codex exec` needs `< /dev/null`; owner's opencode auth.json may hold a
`zai` entry opencode ignores (told the owner); local branch cleanup uses `git branch -D` after
merge (tracking makes `-d` refuse).

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
