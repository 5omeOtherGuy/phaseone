# Status — 2026-09-22

Handoff: `/home/phaseonebig/projects/phaseone-collab/fable-orchestrator-prompt.md`; after any
context compaction re-read this file and `DECISIONS.md` first.
Remote: github.com/5omeOtherGuy/phaseone (PUBLIC), trunk-based (`task/*` → `main` on green, push;
D6, D11, ADR-0010); per-worktree seeded targets + rustc semaphore (D20, ADR-0014); only the lead
edits this file (D13). Started 2026-09-19.

**p1 today.** A working, lean Rust coding harness (27 crates; `scripts/gate.sh` green) on five
environments — `claude`, `deepseek`, `deepseek2`, `glm`, `gpt`. A provider is one wire adapter ×
route × model profile composed into ONE runtime `Provider` (ADR-0039); model selection and
mid-session switching are live (ADR-0049); every main agent can start workers with explicit tool
grants (ADR-0050); resume, context control, `finish` and the TUI are mounted.

**Where the lead stands.** `main` = `45411f1` + this commit; ADRs 0001–0053 (0009→0010,
0013→0014, 0026→0050, 0033→0049 superseded). Open work: #46–#48, #53, #54, #12, #45 (owner), #6,
#25.

## Next — READ FIRST

HOW THE LEAD WORKS (owner instructions)
- Implementation goes to DeepSeek workers by default: `scripts/fanout.py <jobs.json>` with jobs
  `{"runner": "p1", "env": "deepseek2", …}`; briefs/outputs in `../phaseone-briefs/`; repair in the
  SAME session (`session` + `prompt_file`); multi-stage workflows with schema-checked outputs and
  resume on `scripts/workflow.py`. The lead keeps specs, ADRs, diff review, lead tests, live checks,
  merges.
- One job ≈ one crate; short briefs, "work in this order, start editing early". A dead run cannot
  move its journal between routes — a fresh session continues from the WORKSPACE ("NOTE ON STATE").
- Accounts: `deepseek2` is the everyday route (owner rule); z.ai GLM is small — `--env glm` sparingly.

LANDING PROCEDURE (one task = one worktree)
1. `scripts/new-worktree.sh <task-slug>`; worker edits; read the diff yourself.
2. Run `scripts/gate.sh` (fmt, clippy `-D warnings`, all tests, core isolation); a test must check
   the expected behaviour, not agree with the implementation.
3. Commit explicit paths on `task/<issue>-<slug>` (never `git add -A`); merge to `main`; push with
   `scripts/push-main.sh` (waits for CI of that commit) after `gh pr checks --watch` on the head.
4. Record accepted runs (`docs/dogfood/runs.jsonl` via `scripts/run-report.py`, evidence in
   `~/.agents/skills/model-cards/evidence.jsonl`); `git worktree remove <path>`.

OPEN ITEMS (issue numbers)
- #46 tool-name coupling: `p1-tui/src/transcript.rs` and `p1-host/src/tui.rs` match literal tool
  names and argument keys, so `apply_patch` is never recognised on GPT — the tool should describe
  its own call target (audit finding 3).
- #47 provider-http helpers: the error-code sanitiser (a security rule, copied twice),
  `http_error_code` and the status→kind table move into `p1-provider-http` (audit finding 2).
- #48 cleanups: explicit worker ordinal, delete the host's copy of credential validation, one
  `ToolFace`, shared provider fixtures in conformance (audit findings 4–8).
- #12 TUI (Kimi K3 session, `in-progress`; coordinate only on the issue; do not edit `p1-tui`):
  the terminal guard inside `p1-tui` contradicts ADR-0043; the `/model` picker must call
  `switch_model` in `p1-host/src/run.rs`, which `p1-tui` does not reference yet.
- #45 re-reading after a context summary — ON HOLD BY THE OWNER (`blocked`); do not dispatch.
- #6 dogfooding group (`ready`): no harness debt left from its list. (unverified: the long-session
  WebSocket upload comparison is still open here.)
- #25 fan-out program (`owner`): research is organised per ADR-0045; nothing active, #45 is queued.

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
  (ADR-0030); `scripts/fanout.py` (ADR-0027), `scripts/workflow.py` + `scripts/audits/modularity.py`,
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

- DONE (ADR-0051, accepted 2026-09-23): a worker without a command tool finishes `done` and the
  host reports it `not verified; parent verification required`; `shell` is never granted
  implicitly. Next: workflows as a module (design with Astra in `../phaseone-briefs/
  workflows-design-*.md`, v1 scope in `workflows-design-v1-scope.md`; engine spikes rhai vs
  rune running; then a Fable 5.1 Claude Code session orchestrates the implementation).
- Risk (untested): the Opus 5.5 preserved-thinking prefix check vs p1's context summarization —
  applies only to Anthropic accounts created on/after 2026-08-31.
- #45 is the owner's; do not dispatch. #46–#48 are `ready` and unclaimed. Small debts:
  `codex exec` needs `< /dev/null`; after a merge use `git branch -D` (tracking makes `-d` refuse).

## Lessons

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
- The stall guard sees only tool-declared writes (#53): a model editing through shell heredocs
  (Opus 5.5 does) looks idle. Until fixed, briefs say "edit with the edit tool".
