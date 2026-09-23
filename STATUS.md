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

**Where the lead stands.** `main` = `604604b`, CI green on that commit; ADRs 0001–0050 (0009→0010,
0013→0014, 0026→0050, 0033→0049 superseded). Open work: #46–#48, #12, #45 (owner), #6, #25.

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

## Workflows (orchestrator session)

Owned by the Fable 5.1 Claude Code orchestrator session (brief:
`../phaseone-briefs/workflows-orchestrator.md`; ADR-0053 proposed; engine rhai per the spikes in
`../phaseone-spikes/`). The orchestrator writes its progress HERE and nowhere else in this file:
- Briefs `../phaseone-briefs/wf1-…wf6-*.md`; questions/answers in `workflows-orchestrator-questions.md`.
- LANDED: `task/workflow-api` — `crates/p1-workflow` API only (settings, `ModelResolver`,
  `StepRunner`, envelope, journal record, `WorkflowService`, `WorkflowObserver`; frozen, additive
  changes only) and fanout's `"model"` job key (→ `p1 --model`). Shipped judge = `claude/claude-fable-5`
  (the Fable profile this route binds), cap 3 on `claude-fable-5`.
- LANDED: job 2 `task/finish-result` (cb51b60, CI green): `OutputContract` + `result` on `finish`,
  `FinishOutcome::structured()`; frozen tests untouched. Job 1 `task/workers-prepared` (this merge):
  `start_prepared`, `wait_for_capacity`, `running`/`max_concurrent`. Both DeepSeek V4.1 Flash, one pass.
- LANDED: job 4 `task/workflow-tools` (this merge): `crates/p1-tool-workflow` (four tools over the
  `WorkflowService` trait) + "# Workflows (only when the user asks)" in every environment prompt.
  gpt-6-sol:high (model-cards trial), one pass, delegated the prompt edits to a sub-worker.
- LANDED: job 3 `task/workflow-engine` (this merge): the rhai engine, journal + prefix replay, caps,
  `InProcessWorkflows`; 28 integration tests + the prompts' example script pinned on the engine
  (`tests/prompt_example.rs`). Opus 5.5 high, one pass. Known: `observer.step_started` fires after
  the step (the worker ref exists only then); `args` can gain keys (never change existing ones).
- LANDED: job 6 `task/workflow-docs` (this merge): `docs/design/workflows.md` (GLM 5.3, one pass;
  its §7 grows with the host glue).
- RUNNING: job 5 `task/workflow-host` — first attempt (Opus medium) stalled at 6 summaries because
  it edited through shell heredocs (issue #53); resumed in the same session at Opus high.
  READY: the audit port (`task/workflow-audit`: `scripts/audits/modularity-prep.py` +
  `modularity.rhai`, engine-tested) and the four live-check scripts
  (`../phaseone-briefs/workflows-live/`). NEXT: land 5, the five live checks, ADR evidence, report.
- FIXED by the lead (8483e64): fanout counted pi-worker wrapper processes; jobs 5–6 use the default pool.

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
