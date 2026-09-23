# p1 performance and maintainability audit — 2026-09-23 (partial; stopped at the quota line)

Owner-directed (via XO) under the operational hold. Numbers first, then the ranked fixes.
Items 1, 2, 4 and 5 are measured; item 3 has two live samples and otherwise needs
instrumentation. Astra's review verdict is folded in at the end.

## 1. Prompt caching — measured on today's 22 headless runs

Source: every `assistant_completed` record's `usage` in `../phaseone-briefs/runs/*-20260923-*/session.jsonl`
(`input_uncached`, `cache_read`); "cached share" = cache_read / (cache_read + input_uncached).

| run | model | requests | input tokens | cached share | share after 1st req | compactions |
|---|---|---|---|---|---|---|
| call-target | deepseek-v4.1-flash | 212 | 38.5 M | 0.993 | 0.993 | 0 |
| role-fallback | deepseek-v4.1-flash | 211 | 36.8 M | 0.980 | 0.980 | 0 |
| spike-rune | deepseek-v4.1-flash | 302 | 47.4 M | 0.991 | 0.991 | 1 |
| spike-rhai | deepseek-v4.1-flash | 224 | 39.2 M | 0.991 | 0.991 | 0 |
| finish-policy | deepseek-v4.1-flash | 190 | 27.3 M | 0.983 | 0.983 | 0 |
| stall-fingerprint | deepseek-v4.1-flash | 118 | 18.2 M | 0.963 | 0.964 | 0 |
| wf-audit-race / wf1 / wf2 | deepseek-v4.1-flash | 87 / 55 / 48 | 7.3 / 3.4 / 3.5 M | 0.987 / 0.977 / 0.950 | same | 0 |
| tui-seams-1 | gpt-6-luna | 156 | 11.6 M | 0.972 | 0.973 | 1 |
| describe-results | gpt-6-sol | 102 | 7.3 M | 0.977 | 0.978 | 1 |
| shadow-hook | gpt-6-sol | 62 | 3.7 M | 0.954 | 0.956 | 1 |
| call-target-continue | gpt-6-luna | 50 | 3.6 M | 0.949 | 0.950 | 1 |
| wf5-repair-schema-hang | gpt-6-sol | 51 | 3.3 M | 0.948 | 0.949 | 1 |
| provider-fixtures | gpt-6-luna | 45 | 1.9 M | 0.939 | 0.941 | 0 |
| wf4-workflow-tools | gpt-6-sol | 36 | 1.6 M | 0.946 | 0.949 | 0 |
| modularity-cleanups | gpt-6-sol | 34 | 1.2 M | 0.874 | 0.877 | 0 |
| provider-http-helpers | gpt-6-luna | 29 | 0.8 M | 0.861 | 0.865 | 0 |
| finish-always | gpt-6-sol | 16 | 0.3 M | 0.823 | 0.833 | 0 |
| wf6-workflow-docs | glm-5.3 | 82 | 6.6 M | 0.968 | 0.968 | 1 |
| wf5-workflow-host | claude-opus-5-5 | 281 | 23.3 M | 1.000 | 1.000 | 11 |
| wf3-workflow-engine | claude-opus-5-5 | 54 | 3.9 M | 1.000 | 1.000 | 2 |

Reading:
- **Steady-state caching works on every route.** Long runs sit at 0.96–0.99; the share is lower
  only on short runs because the first (uncached) request dominates (16 requests → 0.82). There
  is no sign of a per-turn prefix break: "share after the 1st request" equals the overall share
  everywhere, which it could not if something in the prefix changed each turn.
- **What is in the prefix.** The system prompt renders `{{workspace}}`, `{{os}}` and `{{date}}`
  (`environments/*/prompt.md` line 1) — constant within a session, changing once per day; tools
  are assembled in the environment's declared order (stable); no per-turn environment text is
  injected (the host's own continuation and retry messages are ordinary user-role items appended
  at the end). Anthropic gets `cache_control: ephemeral` on the last blocks
  (`p1-provider-anthropic/src/request.rs:198–229`); Codex takes an optional `cache_key`.
- **Anthropic's 1.000 is accounting, not magic**: its `usage` reports cache reads and only 4
  uncached tokens per request, including the summarizer's own request; the Codex and OpenCode
  routes report the true uncached prefix each time it changes.

## 2. Compaction — measured

20 compactions today (`context_replaced` records with `usage`). Trigger: `[context]
summarize_at_tokens` (300 k on deepseek/deepseek2 with a 1 M window, 120 k on claude/gpt,
150 k on glm), keeping `keep_recent_tokens` (60 k on DeepSeek) of the newest history.

| route | compaction request: uncached input / cached / summary output | cached share of the FIRST request after it |
|---|---|---|
| gpt-6-luna (2) | 15 050 / 0 / 975 ; 27 166 / 0 / 1 881 | 0.044 ; 0.042 |
| gpt-6-sol (3) | 24 641 / 0 / 306 ; 21 100 / 0 / 144 ; 17 314 / 0 / 1 163 | 0.141 ; 0.096 ; 0.000 |
| deepseek (1, spike-rune) | 104 236 / 0 / 6 126 | 0.059 |
| glm-5.3 (1) | 26 212 / 0 / 7 662 | 0.066 |
| claude-opus-5-5 (13) | 4 / 0 / 5 176 … 8 701 (all reported cached) | 1.0 (12 of 13), 0.0 once |

Reading:
- **The summarizer's request is fully uncached on every non-Anthropic route** (`cache_read` = 0
  on all seven): it is built as a fresh prompt instead of as the session's own prefix plus one
  instruction. Mean 11.8 k uncached input and 6 k output per compaction; on DeepSeek at 300 k the
  one measured compaction cost 104 k uncached input.
- **After a compaction only the static prefix survives**: the first request afterwards hits 4–14 %
  (the system prompt + tools), which is the expected floor once the history is replaced; nothing
  to gain there beyond making the summary itself cheap.
- **Anthropic compactions are nearly free in tokens** but there were 11 of them in one 281-request
  run (wf5-workflow-host, the run that stalled): each costs a full round trip and, on that run,
  the stall guard's count. The DeepSeek routes compact rarely (300 k) but every request then
  carries ~180 k cached tokens (38 M input over 212 requests) — cheap in money, heavy in latency.

## 3. Latency — two live samples; the rest needs instrumentation

The journal has no timestamps (`assistant_completed`, `tool_started`, `tool_finished` carry
`seq` only) and the stderr trailers carry token counts only, so TTFT and time-to-first-tool-call
per provider/effort cannot be derived from today's 22 runs. Two live interactive probes
(gpt-6-sol at `low`, a one-line prompt, the SLAB TUI in tmux, sampled from `/proc`) give the
only numbers: **time from Enter to the first visible text ≈ 20–22 s in both runs** (0 bytes
written to the terminal during that window; the process was waiting on the response), then
60 numbered lines streamed in ~13 s. Whether those 20 s are the model's reasoning, the Codex
WebSocket route, or p1's prompt build cannot be split without the timestamps of fix 2.

## 4. TUI smoothness — measured live (idle machine, no build load)

Probe: `p1 --env gpt --model gpt/gpt-6-sol:low` in a tmux window, `/proc/<pid>/io` (`wchar`),
`/proc/<pid>/stat` CPU ticks and `VmRSS` sampled every 1–5 s while a 60-line response streamed.
- Process: 2 threads, RSS 23 MB throughout.
- Before the first token: one 21 kB frame (the prompt echo and the working indicator), then
  0 bytes for ~20 s — no redraws while nothing changes, which is the right behaviour.
- While streaming: ~21 kB per 5 s (≈ 4 kB/s) written to the terminal, CPU 2–5 %, the screen
  advancing 4 → 29 → 51 numbered lines across three 5-second samples; no stalls, no bursts.
- Redraw scope: `p1-host/src/tui.rs` draws one frame per event batch and on a worker-sync tick
  (`draw(terminal, …)` at lines 1107 and 1196; `sync_workers` on `tick.tick()`); the write
  volume says the frames are small (diffed by the terminal backend).
- Not measured: frame timing itself (needs a counter: frames per second and time inside `draw`
  per event kind — a `P1_TUI_FRAME_LOG` debug env var would do) and behaviour under the 7 GB
  build load (no builds were allowed during the hold).

## 5. Maintainability after today's landings — measured

Metrics over the changed crates (`src` non-test lines; tests = `tests/` + `#[cfg(test)]`;
`unwrap()`/`expect(` counted outside `#[cfg(test)]`):

| crate | src | test lines | tests | unwrap/expect | notes |
|---|---|---|---|---|---|
| p1-host | 13 535 | 20 495 | 379 | 120 | 3 `allow(dead_code)` (none found by grep in src — attribute in tests) |
| p1-workers | 1 838 | 774 | 5 | **31** | mutex `lock().unwrap()` pattern; scenario coverage lives in `p1-host/tests/worker_*` |
| p1-workflow | 2 627 | 2 568 | 11 | 0 | large scenario tests over a fake runner |
| p1-tool-shell | 3 948 | 3 700 | 129 | 19 | |
| p1-tool-edit / p1-tool-write | 929 / 674 | 529 / 407 | 22 / 17 | 0 | **147 identical lines (44 % of write, 40 % of edit)** |
| p1-tool-patch | 1 708 | 667 | 30 | 0 | |
| p1-tool-finish | 1 217 | 2 113 | 79 | 6 | |
| p1-hook-shadow | 333 | 380 | 10 | 0 | std-only |
| p1-provider-http | 2 996 | 1 817 | 40 | 3 | |
| p1-provider-openai | 4 073 | 5 367 | 131 | 6 | 1 `allow(dead_code)` (`parser.rs:26`) |
| p1-provider-conformance | 1 466 | 724 | 22 | 1 | `#![allow(dead_code)]` on the moved fixtures module (`fixtures/responses.rs:8`) |
| p1-contracts / p1-core | 879 / 1 173 | 69 / 6 720 | 4 / 17 | 0 / 5 | |

Findings:
- **Duplication:** the edit and write tools share 147 of ~300 non-trivial lines each (path
  confinement, read-before-mutate, the `EditPreview`/describe code) — the one real
  duplication today's landings added to. The host's describer and line renderer share no
  line (0 of 60/228), and the provider adapters lost their triplicated error-code helpers
  (#47).
- **Dead paths:** none left from ADR-0057/0059 (the only remaining name match is on the
  tool-supplied verb, `describer.rs:53`); two `allow(dead_code)` attributes deserve a look
  (`p1-provider-openai/src/parser.rs:26`; the whole moved `fixtures/responses.rs` module,
  which should export only what is used).
- **Error handling:** `p1-workers` carries 31 `unwrap`/`expect` in non-test code, nearly all
  `Mutex::lock().unwrap()`; a poisoned lock (a panicking child task, exactly the class the
  ADR-0053 panic guard handles) would take the whole service down with it.
- **Test quality:** the new crates are scenario-tested (workflow engine 28 integration tests,
  shadow hook a test per spec rule, fallback 12 tests, fingerprint with a real git repo); the
  thin spots are `p1-workers` in-crate (5 tests; its behaviour is covered from the host) and
  `p1-tool-delegate` (3 in-crate tests behind 1.4 k lines of scenario tests). No `TODO`,
  `todo!` or `unimplemented!` anywhere in the changed crates.

## Ranked fixes (evidence-based)

| # | fix | expected gain | cost | contract |
|---|---|---|---|---|
| 1 | **Summarize from the session's own prefix**: build the compaction request as the live prompt (same system prompt, tools and history) plus ONE appended instruction so the provider serves it from cache; replacement logic unchanged | per compaction 11.8 k mean / up to 104 k (DeepSeek) uncached input → cached; every compaction's round trip shortens with it | small: `p1-context` request construction; verify `cache_read > 0` in `context_replaced.usage` on Codex/DeepSeek | none (prompt bytes unchanged) |
| 2 | **Timestamps in the journal** (`at_ms` monotonic on every record; `request_sent_ms`, `first_token_ms` on completions); `run-report.py` derives TTFT and time-to-first-tool-call per provider/effort | makes the 20 s TTFT samples explainable and every later latency fix measurable | small; each adapter sets two fields | journal contract → ADR draft below |
| 3 | **Merge the edit/write tools' shared core** (confinement, read-before-mutate, preview) into one module used by both (in `p1-workspace`, which both already depend on) | −147 duplicated lines; one place for the write-safety rules | small–medium; behaviour unchanged, tests stay | none |
| 4 | **Poison-safe locks in `p1-workers`** (one helper: `lock().unwrap_or_else(PoisonError::into_inner)`) | a panicking child can no longer take the worker service down | small | none |
| 5 | **Tune `summarize_at_tokens` per route from data, after fix 1** (DeepSeek at 300 k re-sends ~180 k tokens per request: 38 M input for 212 requests) | lower TTFT on long DeepSeek runs (quantify with fix 2) | data only | none |
| 6 | **Raise Anthropic's threshold** toward 150 k (Opus 5.5 has 1 M; one run compacted 11 times at 120 k of 200 k) | fewer round trips, fewer stall-guard ticks | data only | none |
| 7 | **TUI frame counter** behind a debug env var (frames/s, time in `draw` per event kind) | turns §4 from "looks fine" into numbers, incl. under build load | small | none |
| 8 | Drop the two `allow(dead_code)` (export only the used fixtures; remove the unused field) | hygiene | trivial | none |

## ADR draft (touches the journal contract)

**Title:** Every journal record carries a monotonic timestamp; completions carry request-sent
and first-token times. **Decision:** `JournalRecord` gains `at_ms: u64` (monotonic since the
session start, never wall-clock, so resume and replay stay deterministic);
`assistant_completed.usage` gains `request_sent_ms` and `first_token_ms` (adapter-set;
`None` when unknown, never 0). Old journals load with `None`. **Consequences:** latency per
provider/effort becomes a run-report field; frozen fixture journals gain the fields only.
Fixes 1, 3–8 change no contract.

## Astra review (gpt-6-astra, xhigh, read-only)

(folded in below when received)
