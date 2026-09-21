# Research #41 — WebSocket transport (used: implemented, opt-in; shipped route stays SSE)

Owner directive 2026-09-21. Decision memo and leaf report: `../phaseone-briefs/research/41/`.
ADR-0047, specification `docs/design/websocket.md`. Live checks by the lead, 2026-09-21, on
`environments/gpt` (`gpt-5.6-sol`, ChatGPT/Codex subscription).

## What exists now

- `p1-provider-http::ws`: connector seam, `tokio-tungstenite` connector, scripted peer (stage A).
- `p1-provider-openai`: WebSocket framing with SSE fallback (stage B) and continuation —
  `previous_response_id` plus only the new items (stage C). Opt-in per route:
  `transport = "websocket"` under `[adapter_settings]`.
- Only the Codex route can use it: no vendor documents a WebSocket for the Anthropic
  subscription, OpenCode Go or the z.ai coding endpoint (leaf-reported; reopen per route when a
  vendor documents one).

## Live findings

1. **The subscription backend accepts the upgrade** (`wss://chatgpt.com/backend-api/codex/responses`,
   `OpenAI-Beta: responses_websockets=2026-02-06`): a text turn, a tool call and its follow-up
   arrived as 48 text frames over ONE reused connection; the existing parser needed no change.
2. **The continuation is accepted**: the follow-up frame went out with `previous_response_id`
   and only the new items, and the response completed normally.
3. **It does not reduce the input tokens the server reports.** The follow-up reported 123 input
   tokens with the continuation — exactly what the SSE path reports for the same turn. The
   server counts the remembered context as input. What shrinks is the UPLOAD, not the usage.
4. **No measurable effect on a small task.** Same fixture task (two failing Python tests), fresh
   workspace per run, all eight runs accepted. Per request `[input_uncached, cache_read]`:

   | Run | Arm | Requests | Wall |
   |---|---|---|---|
   | 1 | SSE | [1642,0] [940,1536] [345,2304] [409,2560] [229,2816] | — |
   | 2 | WS | [1642,0] [940,1536] [366,2304] [408,2560] [235,2816] | — |
   | 3 | WS | [1642,0] [940,1536] [2632,0] [505,2432] [718,2304] | — |
   | 4 | SSE | [1642,0] [940,1536] [361,2304] [432,2560] [3117,0] | — |
   | 5 | WS | 6 requests, one full cache miss | 21.5 s (3.6 s/request) |
   | 6 | SSE | 5 requests, one full cache miss | 17.7 s (3.5 s/request) |
   | 7 | SSE | 5 requests, one full cache miss | 17.0 s (3.4 s/request) |
   | 8 | WS | 5 requests, two full cache misses | 15.6 s (3.1 s/request) |

   Intermittent full cache misses occur in BOTH arms; wall time per request does not differ.

## Disposition

The specification's switch criterion ("`input_uncached` falls on turn ≥ 2") was the wrong
metric — finding 3 — and by it the switch is NOT earned. The shipped Codex route stays
`transport = "sse"`; WebSocket is one line away for anyone who wants it. Untested and the only
place a benefit is still plausible: long sessions, where each SSE turn uploads hundreds of
kilobytes and a continuation uploads a few. Before that comparison p1 needs to SAY which
transport served a request — today a fallback to SSE is invisible (follow-up on #6).
Limits: one account, one day, prompts of 2–3k tokens, two to four runs per arm.

## Owner decision (2026-09-21, after this record)

"Set transport to websocket as default with reasonable http fallback." The shipped Codex route
sets `transport = "websocket"` (merged with the ws-default job); transient failures before any
visible output retry up to the retry budget with backoff before falling back to SSE, modelled on
the donor's `WsRecoveryState`. A live gpt task through the shipped configuration completed and
passed its tests. Still open: a fallback to SSE is not visible to the operator (#6).
