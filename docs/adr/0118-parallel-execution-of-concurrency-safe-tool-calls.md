---
adr: 118
title: Parallel execution of concurrency-safe tool calls
status: proposed
date: 2026-10-06
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [ADR-0023, ADR-0032, ADR-0039, ADR-0051, ADR-0071, ADR-0116, ADR-0117, issue #592, docs/design/core.md, docs/design/journal.md, docs/design/routes.md, openai/codex@c0c230e6730b3b3c9101b8aff4b9aea4027cea5b, code.claude.com/docs/en/agent-sdk/agent-loop, code.claude.com/docs/en/env-vars, platform.claude.com/docs/en/agents-and-tools/tool-use/parallel-tool-use]
---
# ADR-0118: Parallel execution of concurrency-safe tool calls

## Context

Owner order 2026-10-06 (issue #592): let p1 run the tool calls of one response concurrently and
settle cancellation, journal order and conflicting calls; "each adapter must behave exactly like
the original in Claude Code for Anthropic and Codex for OpenAI, to make them as close to native
as possible". Models already return several calls per response on all three wire formats (969
such responses in the journals, issue #592), and the core runs them one after another
(`crates/p1-core/src/lib.rs:466`, step 3g).

ADR-0023 decides two things. This ADR replaces only its sentence "Tools run strictly
sequentially, in block order"; its cancellation precedence (core.md R1) and "the core awaits a
running tool rather than dropping it" (D18) stand, so ADR-0023 is cited, not superseded.

What the reference clients do (labels: source-verified = read in source at the cited commit;
documented = public documentation; observed = seen in a run; unverified = no evidence either way):

- **Codex** (`openai/codex` at `c0c230e6730b3b3c9101b8aff4b9aea4027cea5b`, 2026-10-06, paths under
  `codex-rs/`), all source-verified:
  - Every Responses request carries `"tool_choice":"auto"` and `"parallel_tool_calls"`:
    `core/src/session/turn.rs:1586` sets the prompt's flag to `true`, and
    `core/src/client.rs:1000-1001` sends `prompt.parallel_tool_calls && !model_info.use_responses_lite`.
    Neither field is skipped when serialized, over HTTP or WebSocket
    (`codex-api/src/common.rs:289-290`, `:343-344`). The backend is the ChatGPT one,
    `https://chatgpt.com/backend-api/codex/responses` (`cli/src/doctor.rs:3750`).
  - The value is per model. The bundled catalog marks `use_responses_lite: true`, so sends
    `parallel_tool_calls: false`, for gpt-6-astra, gpt-6.1-sol, gpt-6-sol, gpt-6-luna,
    gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna (`models-manager/models.json:23,196,372,543,710,851,992`),
    and `false`, so sends `true`, only for gpt-5.5 (`:1359`). The live catalog comes from the
    backend's `/models` (`models-manager/src/manager.rs:285`); its current values are unverified.
  - This contradicts `routes.md`'s donor constraint "no `tool_choice`, no `parallel_tool_calls`
    ... over HTTP" as far as sending the fields goes; that the backend accepts them is inferred
    from Codex shipping them, not observed by p1.
  - Codex has no Chat Completions wire: `WireApi` has only `Responses`
    (`model-provider-info/src/lib.rs:107-111`).
  - Overlap is a per-tool flag, default `false` (`tools/src/tool_executor.rs:122-124`), enforced
    by one turn-wide `RwLock`: a parallel-capable call takes the read lock, any other the write
    lock (`core/src/tools/parallel.rs:50,205-209`). Parallel-capable: `exec_command`
    (`core/src/tools/handlers/unified_exec/exec_command.rs:142-143`), `write_stdin`, `view_image`,
    `tool_search`, MCP resource reads, and MCP tools with a read-only hint or a server opt-in
    (`core/src/tools/handlers/mcp.rs:148-151`). `apply_patch`, agent tools and
    `request_user_input` keep the default `false`. There is no cap on concurrent readers.
  - Calls start while the response still streams, at each `OutputItemDone`
    (`core/src/session/turn.rs:2686,2790`); each acquires the lock inside its own spawned task,
    so lock order follows start order only approximately. Results go back in call order: a
    `FuturesOrdered` drained after the stream ends (`turn.rs:2587,2467-2490,3164-3172`).
  - Interrupt: the stream loop breaks with `TurnAborted` (`turn.rs:2636-2639`), the in-flight
    calls are still drained in order, and each running call's task is ABORTED unless it already
    reached a terminal outcome; the call gets the synthesized result `aborted by user after
    <s>s` (`Wall time: <s> seconds\naborted by user` for `exec_command`)
    (`core/src/tools/parallel.rs:249-280,341-346`).
- **Claude Code** (closed source; public documentation only):
  - Read-only tools (`Read`, `Glob`, `Grep`, MCP tools marked read-only) can run concurrently;
    `Edit`, `Write` and `Bash` run sequentially; custom tools are sequential unless they set
    `readOnlyHint` (documented, code.claude.com/docs/en/agent-sdk/agent-loop, "Parallel tool
    execution").
  - At most 10 read-only tools and subagents run at once by default,
    `CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY` (documented, code.claude.com/docs/en/env-vars).
  - It does not disable parallel tool use: several `tool_use` blocks of one response were issued
    and executed in this design session (observed, 2026-10-06). Whether it sends
    `tool_choice` or `disable_parallel_tool_use: false` explicitly: unverified.
  - How it orders results, groups read-only and state-changing calls of one response, and
    treats running calls on interrupt: unverified.
- **Anthropic API** (documented, platform.claude.com parallel-tool-use): parallel tool use is on
  by default; `disable_parallel_tool_use: true` inside `tool_choice` limits a response to one
  call; the API prescribes no execution order; the client returns one `tool_result` per
  `tool_use` id, all in the next user message, before any text; a call not run still gets a
  result with `is_error: true`.
- **DeepSeek** chat completions document no `parallel_tool_calls` parameter; `tool_choice: auto`
  lets the model call "one or more" tools (documented, api-docs.deepseek.com/api/create-chat-completion).

p1 today (source-verified in this repository): the Anthropic adapter sends no `tool_choice` (the
string occurs nowhere in `crates/p1-provider-anthropic/src`) and coalesces adjacent tool results into one
user message (`:446`, `build_messages`); the Responses adapter asserts both fields absent
(`crates/p1-provider-openai/src/request.rs:1033-1034`); the chat adapter sends neither
(`crates/p1-provider-openai-chat/src/request.rs:224,251`). Every tool module runs on a fresh
Store per call and the executor already runs calls concurrently
(`crates/p1-module-runtime/src/executor.rs:5-13`).

## Decision

1. **Concurrency is a per-call tool property; the core schedules.** `p1_contracts::Tool` gains
   `fn concurrency(&self, call: &ToolCall) -> Concurrency { Concurrency::Exclusive }`, with
   `enum Concurrency { Shared, Exclusive }`. The default is Exclusive, as in both reference
   clients. The core treats a call as Shared only when `concurrency(call)` is Shared AND
   `effect(call)` is `Effect::ReadOnly`; anything else is Exclusive. The core names no tool and
   no provider. Module tools need no WIT or manifest change: `WasmTool` answers Shared exactly
   when the call's effect is ReadOnly and its manifest grants nothing outside the observe-only
   interfaces `types`, `control`, `clock`, `random`, `notices`, `workspace`, `snapshot`,
   `directory-listing`, `tool-outputs`. Shipped result: `read`, `search`, `ls`, `read_output`
   are Shared; `edit`, `write`, `apply_patch`, `shell`, `shell_job` (`process-jobs`),
   `ask_user_question` (`user-questions`), `finish` (`completion`) and every worker and workflow
   tool (effect `delegates`) are Exclusive. Every host wrapper of `Tool` forwards `concurrency`.
2. **Per-adapter request fields are profile data.** A model profile (ADR-0039) gains the optional
   key `parallel_tool_calls` (bool). Absent: the adapter sends nothing new (today's bodies).
   Present: the Responses adapter sends `"tool_choice":"auto","parallel_tool_calls":<value>`, as
   Codex does; the chat adapter sends `"parallel_tool_calls":<value>`; the Anthropic adapter
   sends `"tool_choice":{"type":"auto","disable_parallel_tool_use":true}` for `false` and
   nothing for `true`. The gpt profiles take Codex's catalog values: `false` for gpt-5.6-sol,
   gpt-5.6-terra, gpt-5.6-luna, gpt-6-sol, gpt-6-luna, gpt-6-astra, `true` for gpt-5.5; a gpt
   profile Codex does not list keeps the key absent. Claude profiles keep it absent (Claude
   Code does not disable parallel use). Chat profiles keep it absent: Codex has no chat wire to
   copy, and DeepSeek documents no such parameter, so the nearest equivalent of Codex is to send
   nothing the server does not already default to. The implementing slice sends the Responses
   fields only after the live check in open question 1 passes; until then the profile key stays
   unset on the subscription route.
3. **Conflicting calls.** After `{AssistantCompleted}` commits (never mid-stream), the calls of
   the response are cut, in block order, into groups: a maximal run of consecutive Shared calls
   is one group; each Exclusive call is a group of its own. Groups run one after another; a group
   starts only when every call of the previous group has its `{ToolFinished}` committed. Any
   pair of calls of which at least one is Exclusive therefore runs in block order, so a
   `read` before an `edit` of the same file completes first and the read-before-mutate check
   sees it; a `read` after a `write` sees the written file. ADR-0032's `WriteGate` and ADR-0117's
   jobs are unchanged: writers, `shell` and `shell_job` are Exclusive, so no p1 call of the same
   agent overlaps a mutation; other agents' mutations race reads exactly as they do today.
   One question prompt and one `finish` at a time follow from Exclusive.
4. **Journal order.** Within a group the core (a) for each call in block order checks `cancel`,
   looks up the tool, asks authorization (raced with `cancel`, as today) and commits
   `{ToolStarted}` for a permitted call; (b) executes the permitted calls concurrently, at most
   10 at once, starting them in block order; (c) commits `{ToolFinished}` for every call of the
   group in block order, including Denied, Unavailable and Cancelled ones, pushing each result to
   the history and emitting `[ToolFinished]` after its commit (R6). All commits come from the
   one turn future, so `seq` stays dense and a store sees the same record order whatever order
   the tools finish in. Results reach the provider in block order. R5 and resume reconciliation
   are unchanged: several started calls without a result each become `Unknown`, the rest
   `Cancelled`, in block order. A failed `{ToolStarted}` commit in step (a) ends the turn before
   any call of the group executes; a failed `{ToolFinished}` commit in step (c) ends the turn
   after every running call of the group has returned.
5. **Cancellation.** ADR-0023 R1 stands. A call whose step (a) sees `cancel` fired gets
   `Cancelled before execution.` and does not start, and every later group's calls get the same.
   A call already executing receives the child token and is awaited, never dropped (D18); it
   records the result it returns. Every call gets exactly one result before the next request.
   p1 does not copy Codex's abort and `aborted by user` text: a dropped future's side effects are
   unknown, which ADR-0023 rejected.
6. **Bounds.** At most 10 calls of one agent execute at once, Claude Code's documented default;
   a larger group starts its next call when one returns (an ordered, buffered join; the core
   spawns no task, core.md §8). Worst case from current constants: 10 Stores x 256 MiB guest
   memory (`MAX_GUEST_MEMORY`, `crates/p1-module-runtime/src/executor.rs:84`) plus 10 x 8 MiB
   read snapshots (tools.md `read`), about 2.6 GiB; the real peak is unknown and is measured by
   the slice (below). Only read-only module calls overlap, so no extra process is ever started.
7. **Tests and measurement** are listed under Consequences; the slice merges only with them.

## Consequences

- Shared calls of one response finish in about the time of the slowest one instead of the sum.
  The gain is unknown until measured: the slice reports, from existing session journals, how
  many responses contain a group of two or more Shared calls, and from a scripted fake-tool run
  the wall time of such a group before and after.
- On the OpenAI subscription route, matching Codex means asking gpt-5.6/6.x for at most one call
  per response (`parallel_tool_calls: false`), which today's omitted field does not; parallel
  execution then helps that route only on gpt-5.5. This is the owner's parity rule applied, and
  it is open question 2.
- Shell calls stay Exclusive on every route, as in Claude Code; Codex runs `exec_command`
  concurrently. One core rule for all routes keeps the journal and the conflict rules the same
  everywhere (open question 3).
- `{ToolFinished}` waits for earlier calls of its group, so a crash can turn a finished fast
  read into `Unknown`; reads are safe to repeat. `[ToolFinished]` events of a group arrive in
  block order, not completion order.
- `core.md` §3g, §4, "Not in this slice" and `routes.md`'s hard-constraint line are updated by
  the implementing slice; `tools.md` gains the concurrency column.
- Open, for the owner or a live probe:
  1. Live check: does the subscription backend accept `tool_choice:"auto"` and
     `parallel_tool_calls` on p1's route today, and what does its `/models` catalog say for
     `use_responses_lite` per model? One request per gpt profile through p1's own route.
  2. Owner: keep exact Codex parity on gpt-5.6/6.x (`parallel_tool_calls: false`, fewer calls per
     response than today), or leave the field absent there?
  3. Owner: Codex runs `exec_command` concurrently; Claude Code runs `Bash` alone. This ADR runs
     `shell` alone on every route. Change only on an owner decision.
  4. Claude Code's result order, grouping of mixed calls and interrupt handling are unverified
     from public sources; this ADR does not depend on them.
- Tests the slice adds, all with explicit synchronization (oneshot/`Notify` gates in fake tools),
  none with sleeps, none with live network:
  1. `[read, read, write, read]`: both first reads are running before either is released; the
     write starts only after both finished; the last read only after the write.
  2. Releasing the second read before the first still journals Started 1, Started 2, Finished 1,
     Finished 2 with dense `seq`, identical on the memory and file journals.
  3. Provider bodies: results in block order; Anthropic one user message with every
     `tool_result` first; Responses `function_call_output` order.
  4. Cancel while a group runs: every running call sees its token, is awaited (the fake returns
     only when released) and records its own result; later groups get `Cancelled before
     execution.`; one result per call; `TurnEnd::Cancelled`.
  5. Cancel before a group: no `{ToolStarted}`, every call Cancelled; authorization not asked.
  6. Commit failure at the second `{ToolStarted}` and at the first `{ToolFinished}` of a group;
     the next turn's R5 yields `Unknown` for started calls, `Cancelled` for the rest, in block
     order; resume from the same records gives the same.
  7. Twelve Shared calls: never more than 10 in flight (counted by the fake), twelve results in
     block order.
  8. A tool that keeps the default, and one that says Shared with a non-ReadOnly effect, never
     overlap anything.
  9. Every `Tool` wrapper in `p1-host` and `p1-redact` forwards `concurrency`; `WasmTool`
     classifies each shipped module as in Decision 1.
  10. Golden request bodies for each adapter with the profile key absent, `true` and `false`.

## Alternatives considered

- **Reuse `Effect::ReadOnly` alone.** Smallest, but `finish`, `ask_user_question` and
  `shell_job` status declare ReadOnly for authorization and must not overlap; changing their
  effect would change what policies permit.
- **A new WIT export or manifest key for concurrency.** Explicit per module, but both are frozen
  boundary changes (`wasm-boundary-v1`); the grants already say what a module can touch.
- **Codex's turn-wide read/write lock with calls started mid-stream.** Closest to Codex, but
  `{ToolStarted}` would precede `{AssistantCompleted}` in the journal and lock order would depend
  on task scheduling; the group rule gives the same conflict outcome deterministically.
- **Commit `{ToolFinished}` in completion order.** Shows results sooner, but journal order would
  vary between runs of the same session.
- **Shell as Shared on the OpenAI route only (Codex parity).** Makes the core's schedule depend
  on the route; rejected for now, open question 3.
- **Superseding ADR-0023.** Not needed: its cancellation decision stands.

## Evidence

Codex: `git clone --depth 1 https://github.com/openai/codex` at
`c0c230e6730b3b3c9101b8aff4b9aea4027cea5b` (committed 2026-10-06T12:32:27Z), files and lines as
cited in Context. Claude Code: code.claude.com/docs/en/agent-sdk/agent-loop ("Parallel tool
execution") and code.claude.com/docs/en/env-vars (`CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`), read
2026-10-06; no binary was inspected. Anthropic API:
platform.claude.com/docs/en/agents-and-tools/tool-use/parallel-tool-use, read 2026-10-06.
DeepSeek: api-docs.deepseek.com/api/create-chat-completion, read 2026-10-06. p1: files and lines
cited in Context, at `1bd4b90`.

| Behaviour | Claude Code (Anthropic) | Codex (OpenAI) | p1 after this ADR |
|---|---|---|---|
| Parallel request field | none disabling it, observed; exact fields unverified | `tool_choice:"auto"`, `parallel_tool_calls: !use_responses_lite` (false for gpt-5.6/6.x, true for gpt-5.5), source-verified | profile key: Anthropic absent; Responses as Codex after live check; chat absent |
| Which calls overlap | read-only tools and `readOnlyHint` tools; Edit/Write/Bash sequential, documented | per-tool flag; `exec_command` and read-only tools overlap, `apply_patch` does not, source-verified | ReadOnly effect + observe-only grants; shell Exclusive |
| Ordering of conflicting calls | unverified | turn-wide RwLock, start order approximately block order, source-verified | groups in block order, deterministic |
| Result order to the model | unverified | call order (`FuturesOrdered`), source-verified | block order |
| Concurrency cap | 10 by default, documented | none, source-verified | 10 |
| Interrupt of a running call | unverified | task aborted, `aborted by user after <s>s`, source-verified | awaited with cancelled token, own result (ADR-0023) |
| Calls not yet started on interrupt | unverified | drained; aborted result, source-verified | `Cancelled before execution.` |
| When calls start | unverified | mid-stream at each finished output item, source-verified | after `{AssistantCompleted}` |
