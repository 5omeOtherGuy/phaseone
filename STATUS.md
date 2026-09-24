# Status — 2026-09-22

## Iris TUI migration (#91, slice #93) — 2026-09-24 20:50, Iris lead

Branch `task/iris-tui-migration` (base `5a4d126`). Slice #93 adapts the pinned donor
sanitizer (`iris-agent@5b04a1ad`, `src/ui/textengine.rs`) as `p1-tui/src/text.rs` behind
`Band::render`; frozen `tests/band_sanitize.rs` unchanged. wf6's two low findings and the
wf1 zero-cell residual are fixed with tests. Lead checks: focused p1-tui suites green and
the full guarded gate GREEN (457 s, 1856 tests) on the tree before the one-line residual
fix; evidence in `docs/design/iris-tui-migration-evidence.md`. Landing per the owner's
21:00 order: commit, merge `main`, full gate on the merged tree, PR, cheap-worker PR
review, merge. The live TUI restart stays the owner's decision.
Next: freeze the port scope into slices for the owner's ~50-worker workflow (read-only
survey wf2 is evidence only), write and dry-run its Rhai script, and the plan in
`unified-dashboard-trial/iris-massive-workflow-PLAN.md`. #94–#96 and #98 affect that run.

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

**Where the lead stands.** `main` = this commit; ADRs 0001–0059 all accepted (0009→0010,
0013→0014, 0026→0050, 0033→0049 superseded; 0052 usage ledger and 0056 SLAB TUI are other
sessions'). Open work: #12 TUI (host seams from the handoff §14; §15 owner questions await the
owner), #45 (owner), #6, #25; #46–#48, #53, #54, #60 closed. No worktree of the lead's is live.

## Next — READ FIRST

HOW THE LEAD WORKS (owner instructions)
- Implementation goes to DeepSeek workers by default: `scripts/fanout.py <jobs.json>` with jobs
  `{"runner": "p1", "env": "deepseek2", …}`; briefs/outputs in `../phaseone-briefs/`; repair in the
  SAME session (`session` + `prompt_file`); multi-stage workflows with schema-checked outputs and
  resume through `p1 workflow run` (ADR-0053; the Python `workflow.py` is retired). The lead keeps specs, ADRs, diff review, lead tests, live checks,
  merges.
- One job ≈ one crate; short briefs, "work in this order, start editing early". A dead run cannot
  move its journal between routes — a fresh session continues from the WORKSPACE ("NOTE ON STATE").
- Accounts (XO route notice 2026-09-23 22:50): DeepSeek V4.1 Flash on the primary Go subscription
  (`env: deepseek`) is the main worker again, `deepseek2` its fallback (ADR-0054 chain); Claude-side
  dispatch/review → Fable, never Opus/Sonnet; Astra is consultant/lead, never a worker without an
  owner directive; z.ai GLM off until Friday. HOLD until the Thursday 2026-09-25 19:00 reset.

LANDING PROCEDURE (one task = one worktree)
1. `scripts/new-worktree.sh <task-slug>`; worker edits; read the diff yourself.
2. Run `scripts/gate.sh` (fmt, clippy `-D warnings`, all tests, core isolation); a test must check
   the expected behaviour, not agree with the implementation.
3. Commit explicit paths on `task/<issue>-<slug>` (never `git add -A`); merge to `main`; push with
   `scripts/push-main.sh` (waits for CI of that commit) after `gh pr checks --watch` on the head.
4. Record accepted runs (`docs/dogfood/runs.jsonl` via `scripts/run-report.py`, evidence in
   `~/.agents/skills/model-cards/evidence.jsonl`); `git worktree remove <path>`.

OPEN ITEMS — the ordered list is `docs/design/roadmap.md` (epic #72, children #61–#82, template
issues: current state @ commit, implementation, tests, measurable DoD, state after). Owner priority
2026-09-23: #61 ADR-0060 first; read `~/scratch/p1-next/PLAN.md` (Astra's ADR-0060 audit + spike
protocol, via XO) before dispatching it. Session-log scan done (99 findings → #73–#82,
`docs/research/session-log-scan-2026-09-23.md`). Short form:
1. Perf audit fixes (`docs/design/perf-audit-2026-09-23.md`, corrected ranking): (1) timing
   instrumentation ADR — Enter-to-first-text is ~20 s on Codex low and nobody can say where it goes;
   (2) `scripts/usage-audit.py` (cache share incl. cache_write); (3) TUI draw suppression + frame
   counter (draws unconditionally on 50 ms ticks); (4) compaction experiment behind an ADR
   (non-Anthropic compaction requests are fully uncached); (5) p1-workers critical sections;
   (6) threshold experiments; (7) edit/write extraction; (8) dead-code hygiene.
2. ADR-0060 (proposed): salt spike (two worktrees, one shared target, stub never linked), then
   `local-cargo-config.sh` + `rustc-serial` + profile changes; supersedes ADR-0014.
3. #12 TUI host seams still open in the handoff: §14.3/§14.4/§14.5/§14.10 (coordinate on the
   issue; the TUI session may request changes to the mechanical p1-tui hunks posted on #12);
   §15 owner questions await the owner.
4. Leftovers: one `#[path]` include in `crates/p1-host/tests/credentials_end_to_end.rs`; two
   `allow(dead_code)` allowances; #48 item 5 (shared provider fixtures) if not yet folded in.
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
- NEVER SIGSTOP a build or a worker: a paused rustc keeps its `rustc-serial` slot and every build on
  the machine stalls (3 h lost 2026-09-23). To hold a job back, let its cargo step finish and
  withhold the next; to free disk, remove landed worktrees' `target/` (8–15 GB each).
- Each worktree's `target/` grows to 8–15 GB under clippy + tests; land and remove promptly, and
  start no new worktree while three are live on this disk.
- A rejected push or a CI wait that times out both exit 1 from `push-main.sh`: read the output.
  After a route hits its weekly limit mid-run (DeepSeek 429 GoUsageLimitError), commit the WIP
  by path and continue with a fresh session of another model in the same worktree.
