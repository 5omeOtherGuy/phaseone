# Status — 2026-10-05

Only the lead edits this file (D13). After any context compaction re-read this file and
`DECISIONS.md` first. Remote: github.com/5omeOtherGuy/phaseone (PUBLIC); main requires the
`gate` check (D23). Lead run record: `~/.agents/xo/dispatch/p1-lead-20261004/NOTES.md`.

## Next — READ FIRST

CLEAN SLATE LANDED 2026-10-05 (owner order `<p1_migration_clean_slate_20261001>`; restart "Go"
2026-10-05 00:0x). Every migration change is reviewed, repaired and on main with a green gate,
PRs #554–#579: the eight parked branches, batches B7 B9 B12 B13 B14 B15, G3a-04 (#572), #501
steps 2–3 (#569, #576; ADR-0112/0113), the main flakes #567 (#573) and #568 (#571), #160 (#570,
#578) and #549 (#579: a capture cut short before pipe EOF said `complete`). ADR-0110–0113
accepted. Owner decisions of 2026-10-05: D26–D28. Lead run record:
`~/.agents/xo/dispatch/p1-lead-20261004/NOTES.md`.

HOW THE LEAD WORKS (AGENTS.md is the rule text)
- Workers run in pi/opencode or as Claude Code subagents, never through p1 (`runner: p1` is for
  p1's own dogfood runs). Follow the model-cards skill for routes and evidence.
- Landing (ADR-0107): merge main, `scripts/pre-push.sh`, push, `gh pr create`, at most one
  review (`scripts/review-pr.sh <pr> <focus>`, by level), one repair round, then
  `gh pr merge --auto --squash --delete-branch --match-head-commit <sha>`. The full gate runs in
  CI only (ADR-0105). At most three local builds (D25); targets per task, SSD while 12 GiB free,
  else `/data/build/<task>`.

OPEN, in order
1. #575 aggregate memory bound for workflow scripts (E15, split from #541): Rhai 1.26.1 has no
   allocation hook; options are a child process with a memory limit, lower per-value/variable/
   thread limits, or a counting allocator (needs unsafe). Needs a design and an ADR.
2. #537 G2-15: owner question pending (`~/.agents/xo/for-owner.md`; snapshot C01 "· 3 more" →
   "· 4 more"); candidate patch `workers/B7-run2-G2-15-candidate.patch` in the lead run dir.
3. #577 26 older merged ADRs (0052–0101) still `proposed`; #556 review P2 follow-ups.
4. D27: tools E–J (#512–#517) — ask the owner again now that the clean slate has landed.

## Done (all on main, gate + CI green; history is in git)

- **Core & contracts** — one small core depending only on contracts (ADR-0002, isolation check);
  contracts/state: Send-capable interfaces, flat history, one terminal stream event, opaque
  reasoning replay, usage unknown ≠ zero, journal as the only truth, interrupted calls reconciled,
  confinement + read-before-mutate, session ownership and serialized writes
  (ADR-0015–0019, 0021–0025, 0031/0032).
- **Providers & routes** — adapter × route × model profile in ONE runtime `Provider` (ADR-0039);
  credentials keyed by route with `p1 login`/`logout` (ADR-0040, ADR-0044); ONE conformance suite
  with a seeded-bug self-test per check (ADR-0017); DeepSeek + GLM routes (#9); WebSocket default
  on Codex with a visible SSE fallback (ADR-0047, ADR-0048); an exhausted account as its own error
  kind (ADR-0046); Claude Opus 5.5 live-verified (`f289845`); Codex binds gpt-6-astra, gpt-5.6-sol/
  terra/luna, gpt-5.5 (`sol-mini` refused live).
- **Tools** — one crate per tool, composed only in the composition root (ADR-0004); shell sandbox
  (ADR-0035) + environment allow-list (#4); shell output filters measured on p1 (#42).
- **Workflows (ADR-0053, 2026-09-23)** — `p1-workflow` (rhai engine, Claude Code's script shape,
  roles + per-model caps, journal + replay) and `p1-tool-workflow` on every main agent; `p1
  workflow run`; the modularity audit ported (`scripts/audits/modularity.rhai`). Built by a Fable
  5.1 orchestrator session with one worker per job (DeepSeek, Opus 5.5, gpt-6-sol, GLM); five live
  checks passed. ADR-0051 (a worker without a command tool ends `done — not verified`) preceded it.
- **Audit follow-ups (2026-09-23)** — #54 every task ends with `finish`; #47 one provider error-code
  module in `p1-provider-http`; #48 items 1–4 (explicit ordinal, no host credential copy, one
  `ToolFace`, workers doc); #46 / ADR-0057 tools describe their own call target (`CallDescription`
  with an `EditPreview` for the diff view; host and TUI stop matching tool names). Workers:
  gpt-6-sol and gpt-6-luna at high, all accepted first pass or after one repair.
- **ADR-0059 (2026-09-23)** — tools describe their results (`describe_result`: diff, command,
  matches, files, text) and their own destructiveness; the host describer keeps no tool-name
  table (#60). #48 item 5: shared provider fixtures in `p1-provider-conformance`.
- **ADR-0058 shadow hook (2026-09-23, owner via XO)** — `p1-hook-shadow`, std-only, feature
  `shadow-hook`, mounted via `[shadow] brain_packet_shadow` or PATH; fires detached after every
  committed user input and worker dispatch; live-verified with the brain-tools recipe.
- **ADR-0054 / ADR-0055 (2026-09-23, owner decisions)** — workflow roles carry a fallback chain
  for route failures only (never on a cap); DeepSeek V4.1 Flash is the shipped worker, live-verified
  with the exhausted primary Go route hopping to deepseek2. The stall guard and the `finish` check
  see workspace changes made through shell commands (git-status fingerprint; #53). The Python
  `workflow.py` runner is retired.
- **Delegation** — optional module, machine-wide bounded pool, workers not restored on parent
  resume (ADR-0027, ADR-0034); ADR-0050 supersedes ADR-0026: every main agent has the worker tools;
  a worker gets exactly its parent's grant plus `finish`; conditional prompts, worker report + host
  line, `worker_continue add_tools`.
- **Model selection** — ADR-0049 supersedes ADR-0033: `--model E/P[:effort]`, `--effort`,
  `--models`, `p1 models`, `~/.config/p1/settings.toml`; `Agent::reconfigure`; resume onto another
  model when the provider validates the history; `/model`, `/model REF`, `/effort` through one host
  entry point. Live: 5 cross-model switches.
- **Context & completion** — durable validated summary replacement, turn-level retry, stall guard
  for runs and workers, `finish` + bounded continuation (ADR-0036, ADR-0041, ADR-0042, ADR-0037).
- **TUI** — ADR-0043: pure state machine in `p1-tui`, terminal driver + `FrontEnd` seam in
  `p1-host`; milestones M1–M4c merged by the TUI session (#12).
- **Tooling** — gate as the single definition of green (ADR-0011); ADRs validated in the gate
  (ADR-0030); `scripts/fanout.py` (ADR-0027), `scripts/audits/modularity.rhai` (+ `modularity-prep.py`; the Python
  `workflow.py` retired 2026-09-23 on the owner's decision),
  `scripts/run-report.py` + `docs/dogfood/runs.jsonl` (#8, #26).
- **Research** — ADR-0045 (items end `used` or `discarded`): #26–#28, #35–#37, #41–#43 all closed.
- **Modularity audit 2026-09-22** — `docs/research/modularity-audit-2026-09-22.md`: 104 DeepSeek jobs
  on `1c93432`; layering exact, swap cost low, architecture holds; follow-ups #46–#48.
- **First slice** — every `seams.md` §10 item demonstrated by a command in `docs/SLICE-REPORT.md`;
  the independent review's defects all fixed (`docs/review-2026-09-20-dispositions.md`).

## Workflows (orchestrator session) — CLOSED 2026-09-23

Everything is on main; the session's report is `../phaseone-briefs/workflows-orchestrator-report.md`
(what was built, deviations, live checks, what remains). Remaining items became #53, #54 and a
comment on #46; the ADR's deferred list is unchanged.

## Open decisions and risks

- Open owner decisions: G2-15 (#537, for-owner.md); tools E–J timing (D27).
- Risk (untested): the Opus 5.5 preserved-thinking prefix check vs p1's context summarization —
  applies only to Anthropic accounts created on/after 2026-08-31.

## Lessons

Lessons before 2026-09-30 name retired tools (`scripts/push-main.sh`, a local `gate.sh`,
`runner: p1` workers); ADR-0107 replaced them. They stay as history.

- Bound a worker's test INSIDE the build slot (`build-slot.sh timeout 1200 cargo test`), never
  `timeout … build-slot.sh`: the slot wait is normal and two workers timed out unadmitted
  (2026-10-05). A test binary hung 5 h at 0 % CPU and held a slot: check slot holders each wake.
- `gh pr edit` fails on this repo (Projects classic GraphQL error): change a PR's title/body with
  `gh api -X PATCH repos/<owner>/<repo>/pulls/<n>`; the squash commit takes the PR title.
- After any merge that changes prompts/assembly, rebuild the fanout binary at once
  (`cargo build -p p1-host`), or every p1 fanout job fails at start-up.
- Re-gate (or at least `cargo check --workspace`) after merging `origin/main` BEFORE pushing — a
  push once went out on an unbuilt `main`, and two TUI merges went red the same way.
- Never size `[context]` from one run's peak (split4a: `[context]` 90k, 40 summaries, 698 reads,
  0 edits). Shipped values: 300k of 1M (deepseek, deepseek2), 120k of 200k (claude, gpt), 150k of
  260k (glm); every request re-sends the history, so cache-read dominates cost.
- `pi-worker opus` is never for tool work: 0 tool_calls, invented results.
- Never pipe `scripts/push-main.sh` (`| tail`): the pipe hid a rejected push and a red CI. Read
  its exit code. And merge `origin/main` BEFORE numbering a new ADR (0052 was taken concurrently).
- A hung test held a gate 37 min: `gate.sh` now bounds `cargo test` to an hour; tests that wait
  on a run carry their own timeouts (a hang must be a failure, never a parked gate).
- rhai scripts: never call a closure held in a variable or write a captured variable inside a
  `parallel`/`pipeline` thunk (rhai `sync` = "Data race detected", timing-dependent); curry the
  inputs, aggregate after the join. The prompts' example says so.
- NEVER SIGSTOP a build or a worker: a paused rustc keeps its `rustc-serial` slot and every build on
  the machine stalls (3 h lost 2026-09-23). To hold a job back, let its cargo step finish and
  withhold the next; to free disk, remove landed worktrees' `target/` (8–15 GB each).
- Each worktree's `target/` grows to 8–15 GB under clippy + tests; land and remove promptly, and
  start no new worktree while three are live on this disk.
- A rejected push or a CI wait that times out both exit 1 from `push-main.sh`: read the output.
  After a route hits its weekly limit mid-run (DeepSeek 429 GoUsageLimitError), commit the WIP
  by path and continue with a fresh session of another model in the same worktree.
