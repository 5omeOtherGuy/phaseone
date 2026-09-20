# The two real provider routes — verified wire shapes

Status legend: **[donor]** read from iris-agent@62c8345 source and its unit tests (the
owner ran these routes in production) · **[live]** confirmed by a p1 live smoke check ·
**[todo-live]** must be confirmed live before the adapter relies on it.

Both first-slice routes are *subscription* routes (OAuth), not public API-key routes.
They differ from the public APIs in endpoint, headers and a few hard constraints;
those differences are listed explicitly because public documentation does not cover them.
No credential values appear here or in any fixture; fixtures are hand-written,
real-shaped SSE transcripts, never captured authenticated traffic.

## A. Claude subscription — Anthropic Messages  (`anthropic-messages`)

**Request** [donor] `POST https://api.anthropic.com/v1/messages`, streaming SSE.
Headers: `content-type: application/json`, `accept: text/event-stream`,
`anthropic-version: 2023-06-01`, `user-agent`, `Authorization: Bearer <oauth access token>`,
`anthropic-dangerous-direct-browser-access: true`, `x-app: cli`,
`anthropic-beta: oauth-2025-04-20,claude-code-20250219` (+ `interleaved-thinking-2025-05-14`
only for manual-budget thinking; + `extended-cache-ttl-2025-04-11` only if a 1h TTL is used).
Never `x-api-key` on this route. (API-key route: `x-api-key`, no Bearer, no identity block.)

**Hard constraint** [donor]: `system` is a block array whose FIRST block is exactly
`You are Claude Code, Anthropic's official CLI for Claude.`; the harness prompt is the
second block. Without it the request is rejected. Consequence for p1: the identity block
is *provider wire behaviour* (added by the adapter for this route, reported in the
resolved environment manifest), not part of the prompt file.

**Body** [donor] `model`, `max_tokens` (model output cap), `stream:true`, `system:[blocks]`,
`messages`, `tools` (omitted when empty), optional thinking. No `metadata`, no `tool_choice`.
- Tool declaration: `{"name","description","input_schema"}` — JSON-schema function tools only.
- Thinking: adaptive models → `thinking:{"type":"adaptive","display":"summarized"}` +
  `output_config:{"effort":"low|medium|high|xhigh|max"}`; manual-budget models →
  `thinking:{"type":"enabled","budget_tokens":N}` with `1024 <= N < max_tokens`. Off → key absent.
- `cache_control:{"type":"ephemeral"}` on: last system block, last tool declaration, last
  content block of the last user message.
- Messages strictly alternate user/assistant; adjacent same-role items coalesce into one
  message's `content[]`. Tool result = user-role block
  `{"type":"tool_result","tool_use_id","content","is_error"}`; consecutive results share one
  user message and following user text is appended to it. Assistant text + `tool_use` blocks
  share one assistant message.

**Stream** [donor] `message_start` (id, model, baseline usage) · `content_block_start`
(`text` | `thinking` | `redacted_thinking` | `tool_use`{id,name}) · `content_block_delta`
(`text_delta` | `input_json_delta` | `thinking_delta` | `signature_delta`) ·
`content_block_stop` · `message_delta` (`stop_reason`, final usage) · `message_stop` ·
`ping` (ignore) · `error` (report `error.type`).
- A tool call is complete only at ITS `content_block_stop`; arguments are the concatenated
  `partial_json` (empty buffer = `{}`); unparsable JSON is an explicit error, never repaired.
- EOF without `message_stop`, or with any open block, is a failure — never an implicit
  completion. A contentless stream with a valid `stop_reason` is a legitimate empty turn.
- `stop_reason`: `end_turn | tool_use | max_tokens | model_context_window_exceeded |
  stop_sequence | pause_turn | refusal`, anything else → other.

**Replay** [donor] Thinking blocks go back byte-exact:
`{"type":"thinking","thinking":<text>,"signature":<sig>}` (even with empty text);
`{"type":"redacted_thinking","data":<data>}`. Only for the same route + model that
produced them; foreign-origin reasoning is dropped, not downgraded. → p1: opaque replay
data tagged with origin `{route, model}` and a version (seams §3).

**Usage** [donor] `input_tokens`, `output_tokens`, `cache_read_input_tokens`,
`cache_creation_input_tokens` (+ `cache_creation.ephemeral_5m/1h_input_tokens`), from
`message_start` then overridden by `message_delta`. `input_tokens` EXCLUDES cache reads and
writes on this API. No cost on the wire; a subscription has no per-request price → cost `None`.

**Auth** [donor] Reuses the owner's existing Claude Code login: `$CLAUDE_CONFIG_DIR/.credentials.json`
else `~/.claude/.credentials.json`. Refresh: `POST https://platform.claude.com/v1/oauth/token`,
header `anthropic-beta: oauth-2025-04-20`, JSON `{grant_type:"refresh_token", refresh_token,
client_id, scope}`; the refresh token ROTATES and must be written back to the same file
atomically (another tool — Claude Code itself — shares it). 401/403 → one forced refresh, once.

## B. ChatGPT/Codex subscription — OpenAI Responses  (`openai-codex-responses`)

**Request** [donor] `POST https://chatgpt.com/backend-api/codex/responses`, streaming SSE
(the donor's WebSocket transport is NOT taken). Headers: `Authorization: Bearer <access>`,
`chatgpt-account-id: <from the token's JWT claim https://api.openai.com/auth .chatgpt_account_id>`,
`originator`, `User-Agent`, `OpenAI-Beta: responses=experimental`, `Content-Type: application/json`.
The public `api.openai.com/v1/responses` API-key route is a different route, not built in this slice.

**Body** [donor] `{"model","store":false,"stream":true,"instructions":<prompt>,"input":[…],
"tools":[…],"text":{"verbosity":"low"}}` plus, when reasoning is on,
`"reasoning":{"effort":…,"summary":"auto"}` and `"include":["reasoning.encrypted_content"]`
(also whenever a reasoning item is replayed); `prompt_cache_key` (≤ 64 chars) when caching.
**Hard constraints** [donor]: `max_output_tokens` is rejected (400 `Unsupported parameter`);
no `tool_choice`, no `parallel_tool_calls`, no `previous_response_id` over HTTP.
- Function tool: `{"type":"function","name","description","parameters"}`.
- **[live 2026-09-20]** Freeform patch tool. The donor never declared one; p1 does:
  `{"type":"custom","name":"apply_patch","description","format":{"type":"grammar","syntax":"lark","definition":…}}`.
  The subscription route ACCEPTS it, accepts free-form `instructions`, and `gpt-5.6-sol`
  answered with a `custom_tool_call` whose `input` was a raw V4A patch
  (`*** Begin Patch\n*** Add File: hello.txt\n+hello\n*** End Patch\n`). No function-face
  fallback is needed on this route (the face stays for routes without freeform tools).

**Stream** [donor] `response.created` (id) · `response.output_item.added` ·
`response.output_text.delta` · `response.reasoning_summary_text.delta` /
`response.reasoning_summary_part.added` (display only) · `response.custom_tool_call_input.delta`
(display only) · `response.output_item.done` — **the completion unit**: `message`, `reasoning`
(`encrypted_content`), `function_call` {`call_id`,`name`,`arguments`: JSON string},
`custom_tool_call` · `response.completed` (final envelope + usage) · `response.failed` /
`error` · `response.incomplete`.
- `function_call_arguments.delta` is NOT used to build calls; a call exists only at its
  `output_item.done`. The donor turned unparsable arguments into `{}` — p1 does NOT:
  invalid arguments become an explicit tool error (seams §3).
- EOF without `response.completed` is a failure.

**Replay** [donor] With `store:false` no server item ids are echoed. Input items in order:
`{"type":"message","role","content":[{"type":"input_text"|"output_text","text"}]}`,
`{"type":"function_call","call_id","name","arguments"}`,
`{"type":"function_call_output","call_id","output"}`,
`{"type":"reasoning","encrypted_content","summary":[]}` (same route + model only),
and for the patch tool `custom_tool_call` / `custom_tool_call_output`.

**Usage** [donor] `input_tokens` (INCLUDES cached), `input_tokens_details.cached_tokens`,
`output_tokens`, `output_tokens_details.reasoning_tokens`, `total_tokens`; only on
`response.completed`. Cost `None` (subscription).

**Auth** [donor→p1 change] The donor kept its own store (`~/.iris/auth.json`) and login flow.
p1 builds no login flow in this slice: it reuses the owner's existing Codex CLI login
(`$CODEX_HOME/auth.json` else `~/.codex/auth.json`) the same way route A reuses Claude Code's.
Refresh: `POST https://auth.openai.com/oauth/token`, form `grant_type=refresh_token,
refresh_token, client_id`; the refresh token ROTATES → atomic write-back under a file lock,
because the Codex CLI shares the file. **[todo-live]** field layout of that file is read from
the Codex CLI's own source/docs, never by dumping the owner's file.

## C. What the two routes force into the contracts

| Difference | A (Claude) | B (GPT) | Contract consequence |
|---|---|---|---|
| Completion unit of a call | `content_block_stop` of that block | `output_item.done` | Adapter emits a call only when complete; core never sees partial calls |
| Tool input | JSON object | JSON string, or raw text (custom tool) | `ToolInput::Json(raw string) \| ::Text(raw string)`; raw preserved, validated at the tool |
| Declaration kinds | JSON-schema function | function + freeform/grammar | `ToolDeclaration::{Function{schema}, Freeform{grammar}}` + namespaced native options |
| Reasoning replay | text + signature / redacted data | encrypted_content | Opaque `ReplayData{origin{route,model}, version, payload}` per assistant item |
| History layout | alternating messages, coalesced blocks | flat item list | Core history is a flat ordered item list; adapter A coalesces |
| System prompt | block array + mandatory identity block | `instructions` string | Request carries one prompt string; route wrapping is adapter behaviour, shown in the manifest |
| Input-token meaning | excludes cache | includes cache | `Usage` keeps provider fields distinct (`input_uncached`, `cache_read`, `cache_write`), each `Option` |
| Output cap | `max_tokens` required | `max_output_tokens` rejected | Option is route-validated: unsupported explicit option = error, default = adapter's choice |
| Retry | 408/425/429/5xx retry; 401/403 one reauth; `Retry-After` seconds | same | One shared retry helper; never retry after visible output or an executed tool |

Shared, from the donor: SSE chunk decoder (split chunks, multi-line `data:`), status
classification, retry policy (3 retries, 2 s doubling, 60 s cap, jitter). Dropped: WebSocket
transport, native compaction, structured summaries, server-side model fallback,
context-management edits, login flows.

## D. Live smoke checks (lead only: `P1_LIVE=1 cargo test -p p1-live -- --nocapture --test-threads 1`)

2026-09-20, both routes, through `ReqwestTransport` and the owner's existing CLI logins:
text turn, streamed function tool call (`read`), follow-up request carrying the tool result —
all pass on `claude-sonnet-5` and `gpt-5.6-sol`; usage is reported, cost stays unknown.
Observed: Claude `input_tokens` 50/504/571 with cache fields 0 (prompts below the cache
minimum); Codex reports `reasoning_tokens: 0` explicitly.
Reasoning replay: not exercised by these smoke prompts (neither model reasoned on them, even
at high effort). PROVEN LIVE the same day by the end-to-end coding tasks through the host:
both models produced reasoning blocks (Claude `thinking` + signature, Codex
`encrypted_content`) that were replayed across follow-up requests, and across a JSONL
`--resume`, without a rejected request (`docs/SLICE-REPORT.md`).
