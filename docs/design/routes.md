# The subscription provider routes — verified wire shapes

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
only for manual-budget thinking; + `extended-cache-ttl-2025-04-11` only if a 1h TTL is used; + `context-1m-2025-08-07`
only when the route file sets `long_context = true`, ADR-0063).
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
Self-contained since ADR-0061: the shipped route sets `store_only`, so it reads the documented
token and p1's own store and never this file; the borrowed source above survives only for a route
that omits the field (`docs/design/credentials.md` §8).

## B. ChatGPT/Codex subscription — OpenAI Responses  (`openai-codex-responses`)

**Request** [donor] `POST https://chatgpt.com/backend-api/codex/responses`, streaming SSE
(default; a route may opt into the WebSocket transport — ADR-0047, `websocket.md`). Headers: `Authorization: Bearer <access>`,
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
Self-contained since ADR-0061: the shipped route sets `store_only` and reads p1's own store
instead, so this CLI file is not read at runtime; minting p1's own grant is the remaining work
(`docs/design/credentials.md` §8).

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

### Codex prompt caching — measured 2026-09-20 (issue #7)

`prompt_cache_key` in the body alone gave all-or-nothing hits per request (dogfood run 2: 29 %
overall, the first ten requests 0 %). The adapter now also sends the same clamped key as
`session_id` and `conversation_id` headers, as the Codex CLI does. Three interleaved A/B pairs,
same ten-request task, `gpt-5.6-sol`, medium: WITHOUT the headers 9 %, 27 %, 9 % of input
tokens read from cache; WITH them 57 %, 69 %, 64 %. Individual requests still miss (0–17 %)
even with the headers, so routing is only part of the story; the Claude route reaches 97 % on
comparable work. Small sample on one day — re-measure before building on the exact numbers.


## C. DeepSeek on OpenCode Go (`openai-chat/opencode-go-subscription`)

**[docs + live, 2026-09-20]** `POST https://opencode.ai/zen/go/v1/chat/completions`.
Environment `deepseek` selects `deepseek-v4.1-flash`, high effort. This is the Go
subscription endpoint; there is no fallback to Zen pay-as-you-go or another provider.
[OpenCode Go documentation](https://opencode.ai/docs/go/) lists the endpoint and asks
coding clients to identify themselves and supply a stable conversation header. p1 sends
its own `user-agent: p1/<version>` and uses the host's cache key as `x-opencode-session`.
No foreign client identity is impersonated.

Credential precedence (store-only, ADR-0061): `OPENCODE_API_KEY`, then p1's own store entry for
the route id (`p1 login opencode-go-subscription`); no other tool's login file is read. The key is
read by the credential source, never printed or included in the environment manifest.

**Three accounts (2026-09-24, data only).** The owner has three Go subscriptions. Each is its own
route — `opencode-go-1-subscription` (`OPENCODE_GO_1_API_KEY`; allowance used up until 2026-10-07, HTTP 402 until then),
`opencode-go-2-subscription` (`OPENCODE_GO_2_API_KEY`) and `opencode-go-3-subscription`
(`OPENCODE_GO_3_API_KEY`, the current primary) — because p1's store keeps one credential per route
id (ADR-0061), and a separate origin keeps a recorded session's account meaningful. `opencode-go-subscription` keeps its id and `OPENCODE_API_KEY` and is a
compatibility alias for the Go-3 account. Environments `deepseek1` and `deepseek3` name the first
and third accounts; `deepseek` and `deepseek2` are unchanged.

## C2. Free models on OpenCode Zen (`openai-chat/opencode-zen-1`, `-2`, `-3`, alias `-free`)

**[docs + live, 2026-09-24]** `POST https://opencode.ai/zen/v1/chat/completions` — the Zen gateway
itself, one `/v1` up from the Go subscription surface, reached with the same openai-chat adapter,
`thinking-with-reasoning-alias` dialect and `x-opencode-session` header. Each of the owner's three
Zen accounts is its own store-only route with its own variable (`OPENCODE_ZEN_1_API_KEY`,
`_2_`, `_3_`); `opencode-zen-free` keeps its original name and `OPENCODE_ZEN_API_KEY` as the
compatibility alias for Zen-1. Every route binds the same three free wire ids — `space-bunny-free`,
`mimo-v2.6-flash-free` and `muse-spark-1.3-contributor-free` (metadata cost 0 per token, so no paid
fallback exists) — and environments `zen`, `zen2`, `zen3` all default to `space-bunny-free`, so a
workflow can spread work across accounts. **[live, 2026-09-24]** Space Bunny answers every account
key from p1; MiMo and Muse answer HTTP 403 `FreeTierError` ("free tier can only be used from within
OpenCode") on this chat endpoint to a non-OpenCode client, with or without a real Zen key — bound,
but not usable from p1. (The Zen docs list Muse on `/zen/v1/responses`; which endpoint serves it
past the gate is unverified — see its profile.) No Zen usage endpoint is established, so these routes probe as
unsupported (`docs/design/usage.md`). The Muse Spark model's metadata hint (`@ai-sdk/openai`) is
not yet confirmed by a live request on this endpoint.


## C3. ClinePass subscriptions (`openai-chat/cline-pass-1`, `-2`)

**[docs + live, 2026-09-24]** `POST https://api.cline.bot/api/v1/chat/completions` — the documented
way to use a ClinePass subscription outside Cline (docs.cline.bot/getting-started/clinepass.md): an
OpenAI-compatible Chat Completions API, a per-account API key (app.cline.bot → Account → API Keys),
wire ids `cline-pass/<model>`. Each of the owner's two subscriptions is its own store-only route
(`CLINE_PASS_1_API_KEY`, `CLINE_PASS_2_API_KEY`); environments `cline` and `cline2`. Only
`cline-pass/` ids are bound: a bare id (`z-ai/glm-5.3`) is Cline's pay-as-you-go catalogue and bills
Cline credits instead of the subscription (the dashboard showed 0.0000 credits used for the
`cline-pass/` test calls). The stream carries reasoning as the OpenRouter-style `reasoning` delta, so
the routes use `thinking-with-reasoning-alias`. **Owner decision 2026-09-24 21:55:** ClinePass runs
DeepSeek V4.1 Flash and/or GLM-5.3 Flash only; Kimi K3 and the larger GLM/Qwen/MiMo models are not
bound. The routes bind `deepseek-v4.1-flash` (`cline-pass/deepseek-v4.1-flash`, the environments'
default; live 2026-09-24 OK from p1 on both accounts). `cline-pass/glm-5.3-flash` answers 200 too,
but its stream (upstream "AtlasCloud") repeats the finish choice in the final usage chunk, which
p1's chat parser rejects ("choice after finish reason", #115); it joins the routes once that is
fixed (profile: thinking `enabled`, effort `high` only — the vendor turns any other value into
`max`). `cline-pass/deepseek-v4-flash` (listed in the docs) answers 404. Usage is metered
per subscription in a rolling 5-hour, a weekly and a monthly window; no usage endpoint is
established, so the routes probe as unsupported.

## D. GLM on its Z.ai coding subscription (`openai-chat/glm-subscription`)

**[docs + live, 2026-09-20]** `POST https://api.z.ai/api/coding/paas/v4/chat/completions`.
Environment `glm` selects `glm-5.3`, high effort, matching the owner's `glm53` profile
identified by the lead in issue #9. No fallback to the general paid API or Go.
Credentials (store-only, ADR-0061): `ZAI_API_KEY`, otherwise p1's own store entry for
`glm-subscription`; no other tool's login file is read. Construction reads no credential. Access re-reads it; rejection re-reads
once and only retries with a changed key. Static keys have no OAuth refresh; an unchanged
rejected key produces an authentication error. Neither credential source writes files.

[Z.ai Chat Completion](https://docs.z.ai/api-reference/llm/chat-completion) and
[Thinking Mode](https://docs.z.ai/guides/capabilities/thinking-mode) document the message
format and preserved reasoning. p1 sends `thinking: {type: enabled, clear_thinking: false}`
and `reasoning_effort: high`. GLM uses automatic prefix caching: an explicit p1 cache key
is rejected, rather than silently ignored. A preliminary smoke also passed on
`glm-5.3-flash`; that is not the shipped default.

### Shared Chat Completions implementation (C and D)

Following ADR-0039 migration step 2, `p1-provider-openai-chat` takes an injected
`ChatRoute`, configured wire model, `Arc<ModelProfile>`, transport and `CredentialSource`.
The closed subscription-route enum is gone. `ChatRoute` carries origin, endpoint,
non-secret headers, optional session header, wire dialect and route output ceiling.
`ChatDialect` names implemented encodings (`ThinkingWithReasoningAlias` or
`RetainedThinking`), never vendors. Both dialects accept `reasoning` as a streaming alias for
replayable `reasoning_content`; the retained dialect additionally supports preserved thinking
and streaming function inputs. The constructor rejects incompatible continuation requirements,
unsupported efforts, credential-bearing URLs/known credential headers and invalid route settings.

The additive `p1-model-profile` crate depends only on contracts. It holds the currently
consumed model policy: identity, enabled/preserved thinking, supported/default efforts and
output ceiling. The same profile can bind to two compatible endpoints without changing
model-related request fields; route limits may narrow, never enlarge, its allowance.
The host catalog supplies the two bindings and owns borrowed credential lookup in `auth`.
Live smoke tests call those same host constructors. The p1 credential store is deferred:
precedence today is explicit environment variable then borrowed CLI login, with the future
store slot between them (ADR-0040). No credential writes are performed.

Environment profile selection and data-driven route files are migration step 3, not part
of this change. Both catalog keys, environments and origin strings stay unchanged.
The runtime Provider/core/HTTP/tool seams stay unchanged, with no new third-party dependencies.
Both environments assemble read/edit/write/grep/shell/finish, with separate family prompts.

**Request:** system/user/assistant/tool messages; function declarations under
`tools[].function`; raw JSON arguments remain strings. Inbox items use user messages.
Tool results retain their call IDs and exact content. Freeform declarations or history
cannot be encoded and are rejected. `max_output_tokens` maps to positive `max_tokens`.
High is the default; GLM carries low/high/max, DeepSeek high/max; medium/extra-high
are rejected. GLM rejects output caps above its documented 131,072-token maximum.
Unknown options in the adapter namespace are errors. Go enables thinking without GLM's
`clear_thinking` field. Both request streamed usage with `stream_options.include_usage`. GLM also sets
`tool_stream: true` when tools are present to stream argument fragments.

**Stream:** `choices[0].delta.content`, `reasoning_content` (the `reasoning` alias is also
accepted in both dialects), and indexed function-call fragments. Calls retain first-appearance order;
arguments are concatenated byte-exact and never parsed/repaired. `finish_reason` maps
stop/tool_calls/length/content_filter to end-turn/tool-use/output-limit/refusal. Completion
requires `[DONE]` after a finish reason, retaining usage in a later empty-choices chunk.
EOF is failure; output-limited partial calls are not executable. Missing/duplicate call
identities are protocol errors. Diagnostics never copy server error bodies.

**Replay:** each reasoning block carries a version-1 string payload, with configured route
and model as origin. Matching blocks concatenate unchanged into `reasoning_content` in
assistant history. Foreign replay is dropped; unsupported matching replay versions fail.
The model name echoed by a server cannot change the origin.

**Usage:** cache reads come from `prompt_tokens_details.cached_tokens` or
`prompt_cache_hit_tokens`; uncached input is `prompt_cache_miss_tokens`, or prompt total
minus explicitly reported cached input. If the cache split is absent, uncached input is
unknown too. Completion and reasoning tokens remain separate. Cache writes and subscription
cost remain `None`; no notional catalog price is charged as real spend.

**Evidence:** both routes pass the unchanged shared conformance suite, including every-byte
chunk splits, cancellation, retry limits, invalid raw arguments and foreign reasoning.
Additional tests cover request JSON, endpoint/session headers, interleaved calls, truncated
completion, usage splits, credential precedence/rotation and redacted errors. Composition
checks bind one profile to two synthetic routes, reject incompatible dialects, and verify
exact reasoning placement in the second request sent through scripted transport. Fixtures
are hand-written.

Live smoke commands (three requests each; existing subscription credentials):
```
P1_LIVE=1 cargo test -p p1-live deepseek_subscription_route -- --nocapture --test-threads 1
P1_LIVE=1 cargo test -p p1-live glm_subscription_route -- --nocapture --test-threads 1
```
Both passed text, function call and tool-result follow-up on 2026-09-20. DeepSeek emitted
reasoning before the tool call and accepted its replay. With GLM's explicit `tool_stream`
switch, the same smoke yielded five argument fragments and reasoning before the tool call;
the follow-up replays it. Both reported cache reads in these tiny checks; they do not
measure sustained cache efficiency.
Low/max effort, explicit output caps and changed-key retry have synthetic coverage only;
subscription key rejection/rotation has not been induced against the live services.

**Sandboxed coding check, DeepSeek:** `scripts/dogfood.sh routes9-deepseek deepseek .
/tmp/p1-route-checks/task.txt`, with `P1_BIN` pointing to this worktree's own debug build.
Task: fix SSE initial UTF-8 BOM handling, including split BOM bytes, with regression tests.
The disposable p1 clone completed in 150 seconds: exit 0, 25 requests, 27 tool calls,
628,625 input tokens (606,720 cached), 18,227 output tokens; cost unknown. One `finish`
call was rejected for naming checks differently from the actual compound shell commands;
the model reran them standalone and recovered without intervention. New family prompts
now explicitly request standalone verification commands.

Independent acceptance: 54 HTTP tests and formatting passed; a separate Rust harness tested
all three-chunk splits over leading/duplicate BOMs, BOMs inside values, a BOM-only stream
and normal input. Candidate passed; the original baseline failed. Only the intended SSE
file changed. The candidate is not merged as part of route implementation. Local evidence:
`~/projects/phaseone-dogfood/routes9-deepseek.run/report.json`, with the diff alongside it.
This one bounded task does not establish long-run reliability or comparative model quality.

**After the ADR-0039 reshape (b34ba7e, 2026-09-20):** both host-wired live smokes
passed again, with reasoning emitted before tool calls and accepted on follow-up.
Both routes then completed the same bounded SSE BOM task in separate sandboxed clones:

| Route/model | Seconds | Requests | Tool calls | Failed tool calls | Cached input share |
|---|---:|---:|---:|---:|---:|
| Go / deepseek-v4.1-flash, high | 145 | 26 | 32 | 0 | 96.5% |
| Z.ai coding / glm-5.3, high | 434 | 22 | 29 | 0 | 93.7% |

Both exited 0 with no operator intervention or reviewer repairs. Subscription cost remains
unknown. DeepSeek used 630,628 input / 19,849 output tokens; GLM used 697,714 input /
22,720 output tokens. One and two nonzero shell exits respectively were deliberate
negative controls, not provider/tool failures. GLM noticed an insensitive regression fixture
while testing the old behaviour and strengthened it before finishing.

The reviewer independently reran each clone's HTTP tests (54 DeepSeek, 55 GLM), formatting,
and the same standalone adversarial harness covering all two/three-chunk splits of leading,
duplicate and embedded BOMs, BOM-only streams and ordinary streams. Both candidates passed;
the unmodified baseline failed. Each changed only `crates/p1-provider-http/src/sse.rs`.
The generated fixes remain in disposable clones, outside this route PR.

Local evidence directories: `~/projects/phaseone-dogfood/routes9-deepseek-reshaped.run/`
and `~/projects/phaseone-dogfood/routes9-glm.run/`; each has an `accepted-report.json`,
original `report.json` and `changes.diff`. The independent harness is
`/tmp/p1-route-checks/verify.py`. These are integration checks, not a controlled model
comparison or evidence of hours-long reliability.

## E. Kimi K3 on the Kimi coding subscription (`openai-chat/kimi-coding-subscription`)

`POST https://api.kimi.ai/coding/v1/chat/completions` with wire model `k3`.
A non-streaming probe returned both `content` and `reasoning_content`, matching the
GLM coding-plan shape. The `retained-thinking` dialect preserves and replays that
reasoning across turns, including tool turns; `thinking-with-reasoning-alias` cannot
encode the profile's preserved-thinking requirement. Streaming and replay on this
endpoint still need a live check. Credentials are references: `KIMI_API_KEY`, then
Pi's `kimi-coding` login, then OpenCode's `kimi-code-plan-global` login. Do not use
OpenCode's stale `kimi-for-coding` entry (401). The catalog claims a 1,048,576-token
context, but no context capacity has been measured on this route; the profile leaves
it unknown and the environment uses a conservative 260,000-token window pending a
measured run. Subscription cost is unknown, never zero.
