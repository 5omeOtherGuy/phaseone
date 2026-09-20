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

STATE 2026-09-20 ~22:00 — READ THIS FIRST. main = CI green on 61f0a84 (+ local docs commits:
p1-auth spec, ADR-0044 proposed, login spec — push them with the next gated merge).

DONE TODAY (all merged, CI green on the exact commit): Astra's chat adapter + routes (#9);
provider split ADR-0039 steps 1–4 COMPLETE — every shipped provider is wire adapter × route file
× profile file, `WHOLE_PROVIDERS` empty (fakes only), environments name `route`+`profile`;
ADR-0041 turn-level retry (Transport/RateLimited/Protocol); ADR-0042 stall guard
(`--max-idle-summaries`); summary-quality fix; chat-parser tool-type fix; `--sandbox-read`;
fanout p1 runner = full access by default; FrontEnd seam for the TUI (`p1-host/src/frontend.rs`);
agentdash shows p1 runs (brain-tools). Owner set Claude Code to bypassPermissions — no
classifier: AGENTS.md rules are the only guard.

ACCOUNTS: primary opencode-go is OUT OF CREDIT. Use `pi-worker deepseek2 …` and p1
`--env deepseek2` (route `opencode-go-2-subscription`, DeepSeek V4.1 Flash ONLY — owner rule).
p1 jobs need the key in the environment at dispatch:
`OPENCODE_GO_2_API_KEY="$(tr -d '\n' < ~/.config/keys/opencode-go-2.key)" scripts/fanout.py <jobs.json>`
(never print it). z.ai GLM plan is small — `--env glm` sparingly. Context values: deepseek/
deepseek2 summarize at 300k of 1M; glm 150k of 260k. NEVER size [context] from one run's peak
(run split4a thrashed 96 min, 0 edits, under a 90k threshold).

IN FLIGHT:
* `p1-auth` (ADR-0039 step 5): DONE, merged (`bb36fc8`), run recorded. `p1 env show` prints a
  `credential  <source>` line.
* `p1 login` / `p1 logout` (ADR-0044 accepted): DONE, merged. OWNER TO RUN ONCE:
  `p1 login opencode-go-2-subscription < ~/.config/keys/opencode-go-2.key`; after that the
  `OPENCODE_GO_2_API_KEY=…` prefix at dispatch can go (until then keep it).
* `run-report-usage` (research #26): DONE, merged, #26 closed as used. Research batch 1 is fully
  dispositioned (26 used, 27 used, 28 discarded); ADR-0045 accepted. No research item open.
* #30 DONE: `finish` rejects masked checks (`;`, `||`, newline, backgrounding `&`; `2>&1` is
  fine) and reports EVERY failing named command in one error (completion.md §2, last revision).
* `[context]` tables for claude / claude-delegating / gpt: DONE (200k/120k, operational values).
* MAIN WAS RED 23:38–00:05 (2026-09-20): the TUI session merged #32 while red (duplicate
  `ellipsize` in `p1-tui/src/render/screen.rs`); its hotfix #33 (`8a873ec`, 11 deleted lines,
  nothing else) is green. Second red TUI merge that night (#23 before). Rule posted on #12:
  merge only after `gh pr checks --watch` passed on the final head. Before pushing, ALWAYS
  re-gate or at least `cargo check --workspace` after merging origin/main — my 23:45 push went
  out on a main I had not built. Several sessions now edit `p1-tui` (worktrees `12-tui`,
  `12-tui-block-spec`, `neural-home-*`): stay out of those paths.
* TUI: separate Kimi K3 session (tmux window `kimi-tui`, pane %48, worktree `../phaseone-12-tui`),
  coordination ONLY via issue #12. It has merged M1–M4a itself; the seam it needed is on main and
  it may edit exactly two spots of mine: `impl FrontEnd` in its `tui.rs`, the 5-line `--tui`
  branch in `run_agent`. Still owed by the lead, not urgent: a small spec + ADR for
  `ContextStats` (observation-only context budget numbers for the ledger). Check #12 between jobs.

* FAN-OUT PROGRAM (owner request 2026-09-20, issue #25): DECIDED — ADR-0045 (proposed) +
  `docs/design/research-program.md`. Research item = issue labelled `research` + one
  `research:queued|active|decision|implement|used|discarded`; every item ends USED or DISCARDED;
  caps 3 active / 2 in decision / 1 in implement. Curator organises research, NEVER development
  (owner); development, specs, briefs, review, merges stay with the lead.
  BATCH 1 (offline, no build, no live experiment calls): #26 measurement + failure audit,
  #27 context capacity inventory, #28 finish-nudge experiment design. Curator = ONE
  Opus run as a Claude Code subagent (`pi-worker opus` FAILED: tool_calls 0, it wrote tool
  calls as text and invented results — never use it for tool work) (brief `../phaseone-briefs/research-curator-batch1.md`, output
  `../phaseone-briefs/research/curator-batch1.out`, memos `research/<n>/memo.md`), leaves =
  `pi-worker deepseek2`. NEXT: `gh issue list --label research:decision`, decide each memo
  (accept -> lead-owned brief, label `research:implement`; else discard with reopening
  condition). Dev slice A (run-report.py usage aggregation + `scripts/test_run_report.py`)
  enters as #26's implementation. Set ADR-0045 accepted after batch 1 ran under it. After two
  batches: keep the curator layer only if it saves lead effort. Said no for now: websocket,
  benchmark grids, new profile capabilities. Astra's full answer:
  `../phaseone-briefs/fanout-program.answer.md`.

HOW JOBS GO (what worked today): small briefs with a SHORT read-first list and "work in this
order, start editing early"; one job ≈ one crate; p1 runner for implementation, pi-worker for
harness-guard work and other repos; lead reviews the diff, runs the gate independently, merges.
A journal cannot move between routes (ADR-0033): when a run dies, a fresh session continues
from the WORKSPACE — put a "NOTE ON STATE" in the brief.

NEXT, in order:
1. Land p1-auth, then p1 login (above).
2. `[context]` for the claude/gpt environments (no values yet) — #6. Harness debts on #6: the
   chat adapter reports a 401 CreditsError as "key rejected" (surface the error type; distinct
   no-balance message); a run with zero responses prints `in 0 … cost $0.0000`; stall guard
   does not cover delegated workers; `--no-default-features` fails one host test
   (claude-delegating names worker tools).
3. T1 measurement (fixed task set, repeated), host cleanup around the state transitions, model
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
