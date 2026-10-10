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
    "sessionCapabilities":{"close":{}},
    "_meta":{"p1.dev":{"version":1,"extensions":[]}}}}}
```

An unsupported `protocolVersion` gets p1's latest supported version, 1, and the client decides whether to continue.

## The standard surface p1 implements

| Method or update kind | Direction | p1 source | Notes |
| --- | --- | --- | --- |
| `initialize` | client → agent | `p1-acp/src/router.rs`, `capabilities.rs`, `wire/v1` | v1 only. `sessionCapabilities.close`. No `loadSession`, no image, audio or embedded context, no auth methods. |
| `session/new` | client → agent | `router.rs` | Any number per process, each with its own id. `cwd` is the session's folder and must be an existing absolute directory, else invalid params (`-32602`). Without `cwd`: `p1 acp`'s `--workspace`, else invalid params. `mcpServers` is accepted and ignored (#695). The answer carries `configOptions` (below) when the session can switch its model. |
| `session/set_config_option` | client → agent | `p1-acp/src/config_options.rs`, `driver/session.rs`, `p1-host/src/frontend_port/config.rs` | Runs the line mode's `/model` (`configId` `model`) or `/effort` (`thought_level`) switch and answers `{"configOptions":[...]}`, the complete list. Between turns the switch runs at once; a switch that fails is an internal error (`-32603`) and changes nothing. During a prompt the change waits, the answer shows the list as the next turn will run it, and the switch runs before that turn; a waiting change that then fails is reported on p1's stderr, and the update shows the unchanged value. A waiting model switch replaces a waiting effort and removes `thought_level` from the list until it ran. An unknown `configId` or a value the option does not list is invalid params (`-32602`). |
| `session/update` `config_option_update` | agent → client | `driver/session.rs` | The complete list, after every switch that ran. |
| `session/close` | client → agent | `router.rs` | Ends one session as `session/cancel` would, stops its workflow runs and workers, and answers `{}` once the session is gone. Its pending prompt answers `cancelled`. Its id is then unknown (`-32602`). |
| `session/prompt` | client → agent | `driver/session.rs` | Text and `resource_link` blocks only. A link reaches the model as `[name](uri)`. `image`, `audio` and `resource` blocks get invalid params. The answer is the stop reason below, under the hold rule. |
| `session/cancel` | client → agent | `driver/session.rs` | Cancels the turn and every running workflow run and worker, and releases a held prompt. A parked permission request resolves as deny, and the prompt answers `cancelled`. |
| `session/update` `agent_message_chunk` | agent → client | `p1-acp/src/sink.rs` | Assistant text deltas. |
| `session/update` `agent_thought_chunk` | agent → client | `sink.rs` | Reasoning deltas. |
| `session/update` `tool_call` | agent → client | `sink.rs`, `driver/session.rs` | `pending` when the call needs permission, sent before its permission request. `in_progress` when a call starts unannounced. |
| `session/update` `tool_call_update` | agent → client | `sink.rs` | `in_progress` when an announced call is permitted. `completed` or `failed` with the result text. |
| Workflow and worker `tool_call_update` content | agent → client | `workflow_card.rs`, `frontend_port/workflow.rs` | A workflow's initiating card stays `in_progress` until its run ends; cumulative progress lines replace content. Worker end notes append to the initiating card without changing its status, else become assistant text. |
| `session/update` `usage_update` | agent → client | `usage.rs`, `sink.rs` | After each parent response with known input usage and effective context capacity. Latest input-plus-cache tokens as `used`; capacity as `size`; optional cumulative USD cost. |
| `session/update` `plan` | agent → client | `plan.rs`, `sink.rs`, `frontend_port/` | Complete flat snapshot after each workflow-step event. Observed execution steps, not an agent-authored todo list; see the lossy projection below. |
| `session/request_permission` | agent → client | `p1-acp/src/policy.rs` | Options `allow_once`, `allow_always`, `reject_once`. A `cancelled` outcome or an unknown option id denies. Every tool call asks in this slice. |

### Config options

Each option is an ACP `select` (p1 never sends a boolean option), one per category:

- `model` (category `model`): the models in the run's scope (`--models`, else `enabled_models` in `settings.toml`, else every model the environments bind), as `ENV/PROFILE`, plus the running one. The description names the route and the efforts.
- `thought_level` (category `thought_level`): the efforts the running model's profile lists. While the session runs the profile's own setting and the profile names no default effort, a `default` value is listed as current.

A session whose environment names no profile, or whose model the environments no longer list, gets no options. Fixture: [`fixtures/model-switch.jsonl`](fixtures/model-switch.jsonl). Permission modes (#696) will be a further option in the same list.

Every `session/update` and `session/request_permission` carries the `sessionId` of the session it belongs to. A request naming an unknown `sessionId` gets invalid params (`-32602`).

Questions (`ask_user_question`) take p1's headless path until #674. Workers' own activity goes to stderr until #681; their end notes use the standard form below.

### Session usage

The stable ACP v1 [`usage_update`](https://agentclientprotocol.com/protocol/v1/prompt-turn#session-usage-updates) needs no extension declaration. [`fixtures/usage.jsonl`](fixtures/usage.jsonl) records tool-use and text responses, followed by unknown usage and recovery.

- `used` is the latest response's uncached input plus reported cache-read and cache-write tokens, not a lifetime sum. Output and reasoning tokens are not added. Cache categories a route does not report add nothing; unknown uncached input means unknown context usage.
- `size` is the parent's effective context window, including the selected profile's capacity, not its earlier summarization threshold. Unknown input usage or an unknown window sends no update.
- `cost`, when known, is cumulative parent-response spend for that session: `{"amount":0.00325,"currency":"USD"}`. p1 converts micro-USD to USD. A response with unknown usage or cost makes cumulative cost unknown for the rest of that session; subsequent updates omit `cost`, never substitute zero or publish a partial total. A reported known zero remains zero.
- Session state is independent across prompts and sessions. Workers' context and costs are excluded until #681. Usage updates are flushed before the prompt's reply, like other updates.

### Workflow steps as a flat plan

The standard ACP v1 [`plan`](https://agentclientprotocol.com/protocol/v1/agent-plan) needs no extension declaration. p1 has no agent todo tool: the source is the workflow engine's existing step start/end observations and `RunReport.steps`, forwarded through the neutral front-end port. The separate todo-tool gap is not implemented here.

- Each update replaces the complete plan for the session: all observed workflow steps, including earlier runs and repeated start events. Rows are keyed by run + ordinal (the run's `agent()` call order), not call or worker id. Runs keep start order; steps keep ordinal order. A fallback updates the same row. State never crosses sessions.
- `content` is `<run>/<ordinal>: <label>`, falling back to the script's task text. An end-only replay or refusal uses its label, else call id; an existing row retains its task text when the end event has none. This is not the worker's assembled prompt.
- Running steps have `status: "in_progress"`; successful `done` steps have `status: "completed"`. Failed, blocked and cancelled goals remain unfinished: they have `status: "pending"` and an explicit `(failed)`, `(blocked)` or `(cancelled)` suffix in content. **This is lossy:** ACP's three states cannot encode those terminal execution outcomes. Here `pending` means the goal remains unfinished, not a promise that p1 will retry it. They are never marked successfully completed.
- Every entry has `priority: "medium"`: workflow state has no relative priorities, so the projection gives all steps equal priority. Queued-job counts are not fabricated into pending tasks; steps appear only when observed. This is execution progress, not an advance plan.
- Step-end updates are queued before releasing a held prompt. [`fixtures/plan.jsonl`](fixtures/plan.jsonl) freezes only the plan notifications from a real-host two-step workflow (done, then blocked). Its test drives initialization, prompt and approvals; unrelated parent output and permission traffic are not frozen because they can interleave with the background workflow.
- The detailed workflow tree remains the scope of #680 (`_p1/workflow_update`), not a new p1-specific shape in this slice. Workers' own activity still goes to stderr until #681.

### Workflow cards and worker end notes

These are standard ACP v1 `tool_call_update` notifications, with no extension declaration. [`fixtures/workflow-run.jsonl`](fixtures/workflow-run.jsonl) freezes the initiating cards from a real-host two-step workflow followed by a direct worker. The test drives two held prompts and approvals separately; unrelated background traffic is not frozen.

- A successful workflow start result links the run id to its initiating `toolCallId`. The card remains `in_progress`, even though the start tool returned. Run start, phase, log, queued-job count, step start/end, thunk failure and run-end summary append lines to this card. Phase, log and end lines retain the host's existing wording. Each content update replaces the cumulative text, not just the newest line; observations arriving before the start result are buffered and replayed in order.
- The final update marks `completed` only for outcome `completed`. `completed_with_issues`, `failed` and `cancelled` become `failed`, with the actual outcome retained in the summary. **This is lossy:** ACP's tool statuses do not distinguish those run outcomes. The final card is queued before releasing the run's hold; the plan projection and hold rule are unchanged.
- A worker end note uses the host's existing `worker_end_note` wording. If its worker id can be linked to a delegation start result, it appends to that initiating card with no `status` field: the already completed start tool is not reopened. Otherwise it becomes an `agent_message_chunk`. Early notes wait for outstanding start results before falling back to assistant text. A reused call id invalidates the old association. Card state is session-local.
- Links recognise assembled workflow/delegation implementation identities, including renamed faces and fixed delegation tools, and parse the tools' `Started workflow <id>` / `Started worker <id> on …` result prefixes. The fixture pins those real tool-result formats so a wording change cannot silently disable linking. Detailed worker activity and workflow trees remain separate future extensions.
- TCK **NOT RUN** for this slice; scripted real-host fixture replay and sink/codec tests are its conformance evidence.

## The hold rule

While a `session/prompt` is pending, a workflow run or delegated worker that starts belongs to it. The prompt stays pending while such work is live. When the work ends, the parent's inbox turn about it runs inside the same prompt, and its updates stream as usual. A background worker reports its end before its notice reaches the inbox, so the prompt also waits for that notice. The prompt answers when the last of these turns ends.

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
