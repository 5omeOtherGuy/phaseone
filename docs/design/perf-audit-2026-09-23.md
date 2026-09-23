# p1 performance and maintainability audit — 2026-09-23 (partial; stopped at the quota line)

Owner-directed (via XO) under the operational hold. Numbers first, then the ranked fixes.
Scope items 1–2 are measured; items 3–5 are NOT done (see "Resume note"): the lead's Claude
weekly window reached the 88 % stop line. No Astra review yet.

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

## 3. Latency — NOT measured (blocked by instrumentation)

The journal has no timestamps (`assistant_completed`, `tool_started`, `tool_finished` carry
`seq` only), and the stderr trailers carry token counts only. Time to first token and time to
first tool call per provider/effort, split into network / prompt build / render, need two
additions before anyone can measure them: a monotonic `at_ms` on every journal record (journal
contract → ADR), and a `first_token_ms` / `request_sent_ms` pair on `assistant_completed` from
the adapter's stream. Until then only wall time per run is known (`docs/dogfood/runs.jsonl`).

## 4. TUI smoothness — NOT measured

Needs a live TTY session with a frame-timing probe (a debug counter of redraws per event and
the time in the render thread); code reading of `p1-tui/src/render` and `p1-host/src/tui.rs`
was not started. The 7 GB build-load behaviour is a live test.

## 5. Maintainability after today's landings — NOT done

Changed crates today: `p1-contracts`, `p1-core` (one forward), `p1-host`, `p1-workers`,
`p1-workflow`, `p1-tool-workflow`, `p1-hook-shadow`, `p1-tool-finish`, every tool crate,
`p1-provider-http`, the three adapters, `p1-provider-conformance`. To audit: duplication
(the describer vs. the line renderer), dead paths after ADR-0057/0059 (old name matches, the
`ToolFace` macros), error handling in the new crates, test quality (fake-runner tests vs.
scenario coverage). Not started.

## Ranked fixes (evidence-based; items 3–5 will add to this list)

| # | fix | expected gain | cost | contract |
|---|---|---|---|---|
| 1 | **Summarize from the session's own prefix**: build the compaction request as the live prompt (same system prompt, same tools, same history) plus ONE appended instruction, so the provider serves it from cache; keep the replacement logic unchanged | per compaction: 11.8 k mean / up to 104 k (DeepSeek) uncached input → cached; one fewer cold request in the loop; latency of every compaction drops with it | small: `p1-context` request construction; verify with the existing `context_replaced` usage — target `cache_read > 0` on Codex/DeepSeek | none (prompt bytes unchanged) |
| 2 | **Timestamps in the journal** (`at_ms` monotonic on every record; `request_sent_ms`, `first_token_ms` on `assistant_completed`); `scripts/run-report.py` derives TTFT and time-to-first-tool-call per provider/effort | makes item 3 measurable; no runtime gain by itself | small; every adapter sets two fields | journal contract → ADR (draft below) |
| 3 | **Tune `summarize_at_tokens` per route from data, after fix 1**: DeepSeek at 300 k means ~180 k tokens re-sent per request (38 M for 212 requests); a lower threshold trades more (then cheap) compactions for shorter requests | lower TTFT on long DeepSeek runs (unquantified until fix 2) | data only (`environments/*/environment.toml`) | none |
| 4 | **Anthropic compaction frequency**: 11 compactions in one run at 120 k of 200 k; raising `summarize_at` toward 150 k (Opus 5.5 has 1 M) halves them | fewer round trips, fewer stall-guard ticks | data only | none |
| 5 | Items 3–5 of the scope | — | — | — |

## ADR draft (touches the journal contract)

**Title:** Every journal record carries a monotonic timestamp; completions carry request-sent
and first-token times. **Decision:** `JournalRecord` gains `at_ms: u64` (monotonic since the
session start, never wall-clock, so resume and replay stay deterministic);
`assistant_completed.usage` gains `request_sent_ms` and `first_token_ms` (adapter-set;
`None` when unknown, never 0). Old journals load with `None`. **Consequences:** latency per
provider/effort becomes a run-report field; the fixture journals in frozen tests gain the
fields only. **Not decided here:** the prompt-contract itself is unchanged by fixes 1, 3, 4.

## Resume note (for the lead's next session, after the Claude reset Thu 17:00)

Done: §1, §2 measured (script inline in the session transcript; re-run over
`../phaseone-briefs/runs/*-20260923-*/session.jsonl`). Next: (a) code-read `p1-context`'s
summarizer request to confirm fix 1's cause; (b) §5 maintainability pass over the changed crates
(cheap: grep + clippy `--all-targets -W dead_code`); (c) §4 TUI code reading and a probe design;
(d) ADR for fix 2; (e) Astra review at xhigh (`codex exec --skip-git-repo-check -m gpt-6-astra
-c model_reasoning_effort='"xhigh"' -s read-only`, wrapped in `usage-meter wrap`, read-only,
max 2 rounds), fold in, summary to XO. Implementation waits for the resets.
