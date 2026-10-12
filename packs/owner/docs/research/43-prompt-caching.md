# Research #43 — prompt caching on the Anthropic and the ChatGPT/Codex route (used)

Decision memo and leaf report: `../phaseone-briefs/research/43/` (not in the repository).
Pinned revision for the reading: `91dee51`; live probe on `e1229fe`, 2026-09-21.

## Findings that stand

- **Anthropic: nothing to change.** Three `cache_control` breakpoints (last tool, last system
  block, last block of the last user message) match the vendor's prefix order; the two recorded
  Claude runs read ~100 % of their input from cache; a context replacement costs ONE breakpoint
  (system and tools are rebuilt identically). Whether the 1-hour TTL is accepted on the
  subscription endpoint is unknown and was not pursued: no need is in evidence.
- **No per-turn prefix churn on either route**: the date in the system prompt is fixed per
  process, tool order comes from an ordered list.
- **Codex, fixed (merged in `77744b3`)**: the parser now reads `cache_write_tokens`
  (`input_uncached = input − cached − cache_write`), and the generated cache key is
  `hash(workspace, environment, agent ordinal)` — stable across resume and re-run; before, a
  process id, a counter and the clock were mixed in, and every resume re-committed the environment.
- The `runs.jsonl` row `read-long-line-gpt-1` (cache share 0.339) predates the session headers
  and must not be cited as today's behaviour.

## Live probe: header names (lead-run, 4 runs, A B B A)

Question: do upstream Codex's header names `session-id` / `thread-id` route to the cache better
than p1's `session_id` / `conversation_id`? Same fixture task (two failing Python tests), fresh
workspace and session per run, `environments/gpt`, all four runs accepted (tests pass).
Per request: `[input_uncached, cache_read]`.

| Run | Arm | Requests | Cached share of all input |
|---|---|---|---|
| 1 | A `session_id` | [1642,0] [940,1536] [341,2304] [375,2560] [3065,0] [316,2816] [257,2944] | 63.7 % |
| 2 | B `session-id` | [1642,0] [2476,0] [2641,0] [1406,1536] [452,2560] | 32.2 % |
| 3 | B `session-id` | [1642,0] [950,1536] [354,2304] [400,2560] [3052,0] | 50.0 % |
| 4 | A `session_id` | [1642,0] [2681,0] [283,2560] [455,2688] [1740,1536] [279,3072] [222,3200] | 64.1 % |

**Result: no support for changing the names** — arm B was not better in either run. Both arms
show intermittent full misses in mid-conversation (a request served with 0 cached tokens after
hits), which fits the vendor's statement that the key influences routing and guarantees nothing.
`cache_write_tokens` was reported as 0 on every request. Limits: two runs per arm, prompts of
2–3k tokens (close to the 1,024-token minimum), one day, one account.

**Disposition:** p1 keeps its header names. Reopen with: a larger matched sample that shows a
difference, or a vendor sentence naming the headers. The intermittent misses are the stronger
lever and are addressed by the WebSocket continuation (ADR-0047 stage C), which stops resending
the prefix at all.
