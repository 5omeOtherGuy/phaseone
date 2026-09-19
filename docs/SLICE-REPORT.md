# Phaseone (p1) — first slice report

2026-09-20. Built in one mostly unattended session by a lead (Claude Fable 5.1 in Claude
Code) orchestrating worker models, test-first. State: `main`, gate green, CI green.
Decisions: `DECISIONS.md` (D1–D20). Specs: `docs/design/`.

## What was built

A working coding harness. `p1 --env claude|gpt "prompt"` runs an agent on a real repository;
each model gets its own route, prompt and tools and sees nothing else.

| Piece | Crates |
|---|---|
| Contracts + agent core | `p1-contracts`, `p1-core` (loop, inbox, cancellation, commit boundaries, projection/resume), `p1-testkit` |
| Providers (wire translation only) | `p1-provider-http` (transport seam, SSE, retry driver), `p1-provider-anthropic` (Claude subscription), `p1-provider-openai` (Codex subscription), `p1-provider-conformance` (the ONE shared suite) |
| Tools (one crate each) | `p1-workspace` (helper), `p1-tool-read`, `-edit`, `-write`, `-search` (grep), `-shell`, `-patch` (apply_patch), `p1-tool-tests` (lead adversarial) |
| Reshaping | `p1-assembly` + `environments/{claude,gpt,claude-delegating}` (config + whole prompt files) |
| Sessions | `p1-journal` (memory + JSONL) |
| Optional delegation | `p1-workers`, `p1-tool-delegate` |
| Host | `p1-host` → binary `p1` (the only crate naming concrete modules), `p1-live` (lead-only live checks) |

21 crates, ≈33k lines of Rust of which roughly half are tests; 554 tests in the gate.

## Acceptance (seams.md §10) — each a command you can run

From `/home/phaseonebig/projects/phaseone`. Live commands use your existing Claude Code and
Codex logins and cost subscription tokens.

1. **Core builds/tests without provider SDKs, file tools, persistence formats or UI.**
   `scripts/check-core-isolation.sh` → `core isolation: ok` (p1-core's dependency graph:
   p1-contracts + tokio/futures/thiserror, 29 packages, no `p1-*` module, no HTTP/TLS/terminal/
   storage crate). `cargo test -p p1-core` → 112 tests against scripted fakes only.
2. **A real coding task on each route with only its intended prompt/tools.**
   `cargo run -q -p p1-host -- env show claude | jq '[.tools[].declaration.name]'` →
   read, edit, write, grep, shell; `… env show gpt …` → shell, apply_patch (freeform).
   Live: `cargo run -q -p p1-host -- --env claude --workspace <repo> --yes "<task>"` and the
   same with `--env gpt`. Done on 2026-09-20 on a scratch Python repo (find a bug, add a CLI
   option with a test, run the suite): both routes exit 0 and the repo's tests pass when
   re-run independently. Claude used read/edit/write/shell, GPT used shell/apply_patch only.
3. **Replace/remove a tool or provider by composition, without editing the loop.**
   `cargo test -p p1-host --no-default-features` (delegation compiled out: 26 tests green, an
   environment naming `worker_start` fails assembly with `UnknownToolModule`);
   `environments/claude-delegating` adds four tools by configuration only;
   `git log --stat -- crates/p1-core/src` shows the loop untouched by every tool/provider commit.
4. **Both adapters pass the shared checks; route-specific fixtures cover the rest.**
   `cargo test -p p1-provider-anthropic --test conformance` and
   `cargo test -p p1-provider-openai --test conformance` — the same 15 checks (ordering,
   partial calls never surfaced, one terminal outcome, cancellation, error paths, unknown usage
   ≠ zero, replay round trip, chunking, retry rules, no credential leaks). The suite proves
   itself: `cargo test -p p1-provider-conformance` runs one seeded bug per check.
5. **Memory and file storage preserve committed state consistently.**
   `cargo test -p p1-journal` — same scripted session on both stores gives identical records
   and projection; `--test lead_crash_resume` cuts a real session file at EVERY byte offset,
   then loads, repairs, resumes and continues: no record lost or invented, nothing re-executed,
   every call paired, sequence dense. Live, 2026-09-20: `p1 --session s.jsonl …` then
   `--resume` — the agent recalled the earlier session (with replayed reasoning), the changed
   prompt was detected and re-committed as a new `Environment` record, sequence stayed dense.
6. **Delegation: a child on the other route completes and wakes its parent; without it the
   harness still works; repair keeps child state.**
   `cargo test -p p1-tool-delegate` (mid-turn / idle / blocked-in-wait / notification dropped;
   child sees only its own prompt and tools; `worker_continue` keeps history);
   `cargo test -p p1-host` test (f) end to end with fakes. Live, 2026-09-20:
   `p1 --env claude-delegating --yes "…start ONE worker in the gpt environment…"` — Claude
   parent started a `gpt-5.6-sol` worker, waited without polling, was notified, read the
   result, verified the files and tests itself, accepted. Exit 0.
7. **Observed memory, tokens, cost** — next section. No targets were invented.

Live provider checks alone: `P1_LIVE=1 cargo test -p p1-live -- --nocapture --test-threads 1`.

## Measured (release build, this laptop, 2026-09-20)

| | p1 |
|---|---|
| Binary | 7.2 MB, stripped |
| Startup incl. full environment assembly (`p1 env show claude`, 50 runs) | 4.5 ms |
| Idle RSS | 5.8–6.3 MB |
| Peak RSS during the real task | 22.6 MB (Claude), 18.7 MB (GPT) |
| Peak RSS, parent + one delegated child (debug build) | 75.7 MB |
| Real task, Claude route (`claude-sonnet-5`, medium) | 25 s · 8 requests · 10 tool calls, 0 failed · in 34,395 (28,808 cached = 84 %) · out 1,666 |
| Real task, GPT route (`gpt-5.6-sol`, medium) | 40 s · 6 requests · 11 tool calls, 0 failed · in 15,620 (2,048 cached) · out 1,639 |
| System prompt | 1,897 chars (Claude), 2,330 chars (GPT) + tool declarations |
| Cost | unknown on both routes (subscriptions report none) — printed as `unknown`, never 0 |

For scale, from the owner's notes (not re-measured here): opencode ≈ 550 MB RSS and ~9k
preamble tokens per job, claude ≈ 300–450 MB. The "model-native beats generic" hypothesis
(T1) is NOT tested by these numbers: one task, one run per route.

Development cost: 13 worker jobs, $4.58 total (deepseek 10 jobs $1.26, sol 2 jobs $2.30,
glm-5.3 1 job $1.02), ≈ 2.6 worker-hours, plus the lead session.

## What came from Iris (iris-agent@62c8345, MIT, same owner)

Copied and adapted, never depended on: path confinement, output bounding, observed-file
registry, read/edit/write/grep/find bodies and tests (`src/tools/`); the one-shot shell path
(`src/tools/bash/mod.rs`); SSE decoding, status classification, retry policy and loop
(`src/mimir/providers/transport.rs`, `retry.rs`); Anthropic Messages and Codex Responses
request builders, stream parsers and their unit tests (`src/mimir/providers/`); OAuth refresh
shapes (`src/mimir/auth/`); the single-selection idea of issue #73; retained-result lifecycle
ideas from `iris-subagent-runtime`. Left behind: UI, the `wayland` tier, compaction,
structured summaries, the prompt-fragment assembler, settings, WebSocket transport, login
flows, server-side fallback, bash sessions/jobs/sandbox, fuzzy edit matching, mythology names.
New in p1: contracts, the core loop, apply_patch, assembly, journal/resume, conformance suite,
workers, host.

## How the work was done (what held up, what did not)

- Spec with numbered steps, exact texts and an explicit invariant list → independent test
  authors (sol, glm-5.3) before implementation → deepseek implements against frozen suites →
  lead reruns, adds adversarial cases, reads the diff. The core passed 44/44 frozen tests
  first time and a held-out third suite found no defect in it.
- Every defect in worker code was found by the lead's adversarial/real-input tests or by live
  runs — none by the worker's own tests: read tool lost its continuation trailer; SSE decoder's
  `finish()` never wired in; unbounded error body; shell exit-code footer cut off by binary
  output; addition-only patch hunk placement; `env show` on delegating environments; no
  prompt-cache key. Recurring pattern: implemented and unit-tested but not wired in.
- Three errors were the LEAD's: red-first tests pushed to `main` (D19); replay keyed on the
  echoed response model in both adapter briefs — caught by the shared conformance suite on
  first contact; and the shared cargo target dir (D12), which let one worktree link another's
  stale build — caught by two workers' handoffs, replaced by D20.
- A live GPT worker ran `pip install --user pytest` under `--yes`. Reverted by the lead; the
  prompts now say to stay inside the repository.

## What is weak

- One small task per route is a demonstration, not evidence of hours-long reliability.
  Owner failures F1/F3/F6 (needless stops, invented limits, forgotten decisions) are addressed
  only by prompt text; F2 (missed completion) is the one with real mechanism and tests.
- No context control: long sessions will simply overflow. The context policy seam exists
  (`ContextReplaced` is journalled) but only a passthrough policy ships.
- `--yes` is all-or-nothing; without it headless mode can only read. No sandbox. The shell
  tool cannot reach a process that escapes its process group.
- Prompt caching on the Codex route is low (13 %) even with a cache key; not investigated.
- Credential refresh takes a blocking file lock on the runtime thread; refresh was not
  exercised live (tokens were valid). Reasoning replay was exercised live on both routes.
- Child usage is not summed into the parent's totals (`usage_total` is always `None`).
- The interactive prompt loop has tests with fake input only; nobody has typed into it yet.
- The host is twice its size guide (≈2k lines) and deserves a read by the owner.

## What should come next

1. Use it for real work for a day and collect failures — that, not more architecture, decides
   the next slice.
2. Context control as a policy module (the owner's F5): measured threshold, model-appropriate
   summarisation through the ordinary provider interface.
3. T1: model-native vs generic environment on a fixed task set (accepted rate, failed tool
   calls, tokens, cache hits), with `claude -p` / `codex exec` as measurement arms.
4. Granular authorization (per-effect, per-path grants) and a worktree-isolated workspace for
   delegated children.
5. Turn-completion policy for unattended runs (F1), and surfacing child usage.
