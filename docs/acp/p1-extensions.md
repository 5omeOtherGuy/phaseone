# p1 over ACP: the published surface

This document is the complete contract between `p1 acp` and its clients. It covers the standard ACP v1 surface p1 implements and every p1-specific extension, now and to come. Anyone must be able to build a client for every p1 capability from this document and the fixtures in [`fixtures/`](fixtures/), without reading p1's Rust (ADR-0154). Its layout follows JetBrains' `air-extensions.md` in [zed-industries/claude-code-acp](https://github.com/zed-industries/claude-code-acp).

## Purpose and scope

p1 implements the agent side of ACP v1 over stdio. Standard ACP comes first, and an ecosystem-agreed shape is preferred over a p1-specific one. A p1 feature with no standard form becomes an extension:

- a `_p1/...` method or notification, or
- a key under `_meta["p1.dev"]`.

Each extension is sent only to a client that declared it.

The first slice has **no extensions**. The negotiation below already works: a declaring client gets the empty extension list.

## Compatibility rule

- A client that does not declare `p1.dev` receives standard ACP v1 only: no `_p1/...` message and no `p1.dev` key. One test per extension proves this, and `crates/p1-acp/tests/driver.rs` proves it for the whole first slice.
- An extension's wire shape never changes within an extension version. A changed shape is a new version, negotiated like the first.
- Every extension names its **standard successor**. When that successor stabilises, p1 sends both forms for one release, then drops the `_p1` form.

## Negotiation

The client declares support at `initialize`:

```json
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{
  "protocolVersion":1,
  "clientCapabilities":{"_meta":{"p1.dev":{"version":1,"capabilities":[]}}}}}
```

p1 answers with the extensions it enables for that client, and only to a declaring client. A malformed or unsupported declaration enables nothing and gets no key.

```json
{"jsonrpc":"2.0","id":0,"result":{
  "protocolVersion":1,"authMethods":[],"agentInfo":{"name":"p1","version":"0.0.1"},
  "agentCapabilities":{"loadSession":false,
    "promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},
    "_meta":{"p1.dev":{"version":1,"extensions":[]}}}}}
```

An unsupported `protocolVersion` gets p1's latest supported version, 1, and the client decides whether to continue.

## The standard surface p1 implements

| Method or update kind | Direction | p1 source | Notes |
| --- | --- | --- | --- |
| `initialize` | client → agent | `p1-acp/src/capabilities.rs`, `wire/v1` | v1 only. No `loadSession`, no image, audio or embedded context, no auth methods. |
| `session/new` | client → agent | `p1-acp/src/driver/session.rs` | One per process; a second call is an error (`-32000`). `cwd` must be the workspace, else invalid params (`-32602`) naming both. `mcpServers` is accepted and ignored (#695). |
| `session/prompt` | client → agent | `driver/session.rs` | Text and `resource_link` blocks only. A link reaches the model as `[name](uri)`. `image`, `audio` and `resource` blocks get invalid params. The answer is the stop reason below, under the hold rule. |
| `session/cancel` | client → agent | `driver/session.rs` | Cancels the turn and every running workflow run and worker, and releases a held prompt. A parked permission request resolves as deny, and the prompt answers `cancelled`. |
| `session/update` `agent_message_chunk` | agent → client | `p1-acp/src/sink.rs` | Assistant text deltas. |
| `session/update` `agent_thought_chunk` | agent → client | `sink.rs` | Reasoning deltas. |
| `session/update` `tool_call` | agent → client | `sink.rs`, `driver/session.rs` | `pending` when the call needs permission, sent before its permission request. `in_progress` when a call starts unannounced. |
| `session/update` `tool_call_update` | agent → client | `sink.rs` | `in_progress` when an announced call is permitted. `completed` or `failed` with the result text. |
| `session/request_permission` | agent → client | `p1-acp/src/policy.rs` | Options `allow_once`, `allow_always`, `reject_once`. A `cancelled` outcome or an unknown option id denies. Every tool call asks in this slice. |

Questions (`ask_user_question`) take p1's headless path until #674. Workers have no ACP form until #681; their activity goes to stderr.

## The hold rule

While a `session/prompt` is pending, a workflow run or delegated worker that starts belongs to it. The prompt stays pending while such work is live. When the work ends, the parent's inbox turn about it runs inside the same prompt, and its updates stream as usual. The prompt answers when the last of these turns ends.

`session/cancel` releases the hold, and so does the next `session/prompt`. The held prompt answers first, then the next one runs. A background shell job never holds.

A notice that reaches the inbox after the hold ended is delivered in the next prompt's turn. `_p1/agent_state` (#683) will add streaming outside a prompt for declaring clients.

## Stop reasons

| p1 turn end | ACP v1 `stopReason` | Rationale |
| --- | --- | --- |
| Completed, end of turn | `end_turn` | The model finished. |
| Completed, `ToolUse` | `end_turn` | A completed turn has no further tool execution to await. |
| Completed, max output tokens | `max_tokens` | The provider hit a token bound. |
| Completed, context window exceeded | `max_tokens` | The provider exhausted a token bound. |
| Completed, `Paused` | `end_turn` | ACP v1 has no resumable-pause outcome. |
| Completed, `Other` | `end_turn` | No more specific terminal reason is known. |
| Completed, refusal | `refusal` | The model refused. |
| Cancelled, or the prompt was cancelled while held | `cancelled` | |
| Provider, journal or context failure | JSON-RPC error `-32603` | The message is p1's safe diagnostic text. |

## `_meta` keys

| Key | Where | Meaning | Since |
| --- | --- | --- | --- |
| *(none yet)* | | | |

## Section template for an extension

Every later extension adds one section in this shape:

### `<name>`

- **Capability:** the string a client lists in `clientCapabilities._meta["p1.dev"].capabilities`, and the one p1 lists in `extensions` when it enables it.
- **Wire shape:** each method, notification or `_meta` key, with a complete example JSON line per direction.
- **Gate:** what a non-declaring client receives instead. Usually nothing, or the standard form.
- **Standard successor:** the ACP shape, RFD or draft that will replace it, with a link, or "none known".
- **Retirement:** the condition for dropping the `_p1` form. Both forms are sent for one release after the successor stabilises.
