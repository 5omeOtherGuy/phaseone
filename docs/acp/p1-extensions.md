# p1 over ACP: the published surface

This document is the complete contract between `p1 acp` and its clients. It covers the standard ACP v1 surface p1 implements and every p1-specific extension, now and to come. Anyone must be able to build a client for every p1 capability from this document and the fixtures in [`fixtures/`](fixtures/), without reading p1's Rust (ADR-0154). Its layout follows JetBrains' `air-extensions.md` in [zed-industries/claude-code-acp](https://github.com/zed-industries/claude-code-acp).

## Purpose and scope

p1 implements the agent side of ACP v1 over stdio. Standard ACP comes first, and an ecosystem-agreed shape is preferred over a p1-specific one. A p1 feature with no standard form becomes an extension:

- a `_p1/...` method or notification, or
- a key under `_meta["p1.dev"]`.

Each extension is sent only to a client that declared it.

The first extension is `workflow_update`. A valid declaration with an empty capability list still gets an empty extension list; a client must name each extension it wants.

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
| `session/new` | client → agent | `router.rs` | Any number per process, each with its own id. `cwd` is the session's folder and must be an existing absolute directory, else invalid params (`-32602`). Without `cwd`: `p1 acp`'s `--workspace`, else invalid params. `mcpServers` is accepted and ignored (#695). The answer carries `configOptions` (below) and `modes` (the permission modes, below). |
| `session/set_config_option` | client → agent | `p1-acp/src/config_options.rs`, `driver/session.rs`, `p1-host/src/frontend_port/config.rs` | Runs the line mode's `/model` (`configId` `model`) or `/effort` (`thought_level`) switch, or changes the permission mode (`mode`), and answers `{"configOptions":[...]}`, the complete list. Between turns the switch runs at once; a switch that fails is an internal error (`-32603`) and changes nothing. During a prompt the change waits, the answer shows the list as the next turn will run it, and the switch runs before that turn; a waiting change that then fails is reported on p1's stderr, and the update shows the unchanged value. A waiting model switch replaces a waiting effort and removes `thought_level` from the list until it ran. An unknown `configId` or a value the option does not list is invalid params (`-32602`). |
| `session/update` `config_option_update` | agent → client | `driver/session.rs` | The complete list, after every switch that ran, sent after the answer of the request that caused it. |
| `session/set_mode` | client → agent | `driver/session.rs`, `p1-host/src/frontend_port/mode.rs` | The same mode change as `set_config_option` `mode`; `modeId` is one of the `availableModes`. Answers `{}`, then sends `config_option_update` and `current_mode_update`. It applies at once, also during a prompt: the next tool call is decided by the new mode. An unknown or missing `modeId`, or a session without modes, is invalid params (`-32602`). |
| `session/update` `current_mode_update` | agent → client | `driver/session.rs` | The new `currentModeId`, after every mode change, by either method. |
| `session/close` | client → agent | `router.rs` | Ends one session as `session/cancel` would, stops its workflow runs and workers, and answers `{}` once the session is gone. Its pending prompt answers `cancelled`. Its id is then unknown (`-32602`). |
| `session/prompt` | client → agent | `driver/session.rs` | Text and `resource_link` blocks only. A link reaches the model as `[name](uri)`. `image`, `audio` and `resource` blocks get invalid params. The answer is the stop reason below, under the hold rule. A prompt that starts with `/` and an advertised command name runs that command instead (see "Slash commands"). |
| `session/cancel` | client → agent | `driver/session.rs` | Cancels the turn and every running workflow run and worker, and releases a held prompt. A parked permission request resolves as deny, and the prompt answers `cancelled`. |
| `session/update` `agent_message_chunk` | agent → client | `p1-acp/src/sink.rs` | Assistant text deltas. |
| `session/update` `agent_thought_chunk` | agent → client | `sink.rs` | Reasoning deltas. |
| `session/update` `tool_call` | agent → client | `sink.rs`, `driver/session.rs` | `pending` when the call needs permission, sent before its permission request. `in_progress` when a call starts unannounced. |
| `session/update` `tool_call_update` | agent → client | `sink.rs` | `in_progress` when an announced call is permitted. `completed` or `failed` with the result text. |
| Workflow and worker `tool_call_update` content | agent → client | `workflow_card.rs`, `frontend_port/workflow.rs` | A workflow's initiating card stays `in_progress` until its run ends; cumulative progress lines replace content. Worker end notes append to the initiating card without changing its status, else become assistant text. |
| `session/update` `usage_update` | agent → client | `usage.rs`, `sink.rs` | After each parent response with known input usage and effective context capacity. Latest input-plus-cache tokens as `used`; capacity as `size`; optional cumulative USD cost. |
| `session/update` `available_commands_update` | agent → client | `p1-acp/src/commands.rs`, `driver/session.rs`, `p1-host/src/frontend_port/commands.rs` | The session's slash commands, right after the `session/new` answer, and again when a switch or a reload changed them. See "Slash commands" below. |
| `session/update` `plan` | agent → client | `plan.rs`, `sink.rs`, `frontend_port/` | Complete flat snapshot after each workflow-step event. Observed execution steps, not an agent-authored todo list; see the lossy projection below. |
| `session/request_permission` | agent → client | `p1-acp/src/policy.rs` | Options `allow_once`, `allow_always`, `reject_once`. A `cancelled` outcome or an unknown option id denies. Asked for every tool call in the `ask` mode, never in the others. |

### Config options

Each option is an ACP `select` (p1 never sends a boolean option), one per category:

- `model` (category `model`): the models in the run's scope (`--models`, else `enabled_models` in `settings.toml`, else every model the environments bind), as `ENV/PROFILE`, plus the running one. The description names the route and the efforts.
- `thought_level` (category `thought_level`): the efforts the running model's profile lists. While the session runs the profile's own setting and the profile names no default effort, a `default` value is listed as current.
- `mode` (category `mode`): the permission mode, below. It is listed also when the session has no `model` or `thought_level` option.

A session whose environment names no profile, or whose model the environments no longer list, gets no `model` or `thought_level` option. Fixture: [`fixtures/model-switch.jsonl`](fixtures/model-switch.jsonl).

### Permission modes

The modes are p1's own policies (ADR-0038), each enforced by the host for the parent and every worker. They are offered both as `modes` on `session/new` with `session/set_mode` and as the `mode` config option; both change the same setting. A change applies to the next tool call, also inside a running prompt.

| Mode | Tool calls | Offered |
| --- | --- | --- |
| `ask` (start) | Every call asks the client (`session/request_permission`). | always |
| `read-only` | Calls that only read run without asking; every call that writes files, executes or delegates is refused without asking: "Not permitted in read-only mode: this call writes, executes or delegates." | always |
| `full-access` | Every call runs without asking, in the shell sandbox `p1 acp` started with. | only when `p1 acp` started without `--ask` |

No mode can widen what the start-up flags allow. `--ask` keeps `full-access` off the list, and the shell sandbox (`--sandbox`) is fixed per process: no mode changes it, and `full-access` names it in its description. Workspace confinement and the credential refusal hold in every mode.

A permission request already put to the client keeps its answer after a mode change; the change decides the calls after it. A call refused without asking (in `read-only`) gets its `tool_call` (status `pending`, no `rawInput`) right before its failed `tool_call_update`. Which calls count as reads is each tool's own declared effect, the same trust `--ask` gives them.

The dsh floor offers `read-only`, `workspace-write` and `danger-full-access`. p1's `read-only` matches dsh's. p1's `full-access` is dsh's `workspace-write` when started with `--sandbox workspace` and dsh's `danger-full-access` when started with `--sandbox off`; p1 has no switch between the two during a session. dsh has no `ask`. Fixture: [`fixtures/modes.jsonl`](fixtures/modes.jsonl).

### Slash commands

The [ACP slash commands](https://agentclientprotocol.com/protocol/v1/slash-commands): the list is closed and built by the host (`SessionHandle::commands`). A prompt runs a command when its text is `/NAME`, alone or followed by whitespace and the argument, and `NAME` is in the last list. Any other text, an unknown `/x` included, is a prompt for the model. Fixture: [`fixtures/slash-commands.jsonl`](fixtures/slash-commands.jsonl).

| Command | Input hint | What it does |
| --- | --- | --- |
| `/model` | `ENV/PROFILE` | Without an argument, the model option's values as text, the current one marked `*`. With one, the same switch as `session/set_config_option` `model`: a `config_option_update`, then `model: E/P` or `model not changed: <reason>`. |
| `/effort` | `level` | The same for `thought_level`. |
| `/compact` | — | The line mode's `/compact` (ADR-0076): one summary of the history now. Reports `compacted: A → B tokens`, `nothing to compact`, or `compact failed: <reason>` (an environment without `[context]` has no summarizer). |
| `/status` | — | Environment, model, route, effort, access, sandbox and workspace, one line each. |
| `/access` | — | The permission mode, the modes this run allows, and the sandbox, fixed per process (ADR-0038). |
| `/modules` | `reload` | `/modules reload`: the line mode's module reload (ADR-0084). |
| `/NAME` for each skill | `what to do (optional)` | Present when the environment assembles the `skill` tool: one command per skill it lists. A turn for the model that asks it to load the skill with that tool and follow it, then the argument. |

`/model` and `/effort` appear only when the session has those config options. Every command but a skill reports as one `agent_message_chunk` and ends the prompt `end_turn` without a model turn; a failure is reported the same way, as text. `session/cancel` reaches a host command, which then answers `cancelled`; `/model` and `/effort` run at once and answer `end_turn`. A skill command's turn is an ordinary prompt turn. A command is a prompt like any other: it waits behind a running prompt and releases a held one (the hold rule). The list is published again after a switch or `/modules reload` that changed it; that update follows the answer of the request that caused it.

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
- The detailed workflow tree uses the negotiated `workflow_update` extension below. Workers' own activity still goes to stderr until #681.

### Workflow cards and worker end notes

These are standard ACP v1 `tool_call_update` notifications, with no extension declaration. [`fixtures/workflow-run.jsonl`](fixtures/workflow-run.jsonl) freezes the initiating cards from a real-host two-step workflow followed by a direct worker. The test drives two held prompts and approvals separately; unrelated background traffic is not frozen.

- A successful workflow start result links the run id to its initiating `toolCallId`. The card remains `in_progress`, even though the start tool returned. Run start, phase, log, queued-job count, step start/end, thunk failure and run-end summary append lines to this card. Phase, log and end lines retain the host's existing wording. Each content update replaces the cumulative text, not just the newest line; observations arriving before the start result are buffered and replayed in order.
- The final update marks `completed` only for outcome `completed`. `completed_with_issues`, `failed` and `cancelled` become `failed`, with the actual outcome retained in the summary. **This is lossy:** ACP's tool statuses do not distinguish those run outcomes. The final card is queued before releasing the run's hold; the plan projection and hold rule are unchanged.
- A worker end note uses the host's existing `worker_end_note` wording. If its worker id can be linked to a delegation start result, it appends to that initiating card with no `status` field: the already completed start tool is not reopened. Otherwise it becomes an `agent_message_chunk`. Early notes wait for outstanding start results before falling back to assistant text. A reused call id invalidates the old association. Card state is session-local.
- Links recognise assembled workflow/delegation implementation identities, including renamed faces and fixed delegation tools, and parse the tools' `Started workflow <id>` / `Started worker <id> on …` result prefixes. The fixture pins those real tool-result formats so a wording change cannot silently disable linking. Detailed worker activity remains a separate future extension; workflow trees use `workflow_update` below.
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
| `p1.dev.version` | `initialize` client capability `_meta`, agent capability `_meta` | Extension contract version, currently `1`. A malformed or unsupported declaration enables nothing and gets no response key. | #673 |
| `p1.dev.capabilities` | `clientCapabilities._meta` | Requested extension names; `workflow_update` opts into the tree since #680. Unknown names are ignored. | #673 |
| `p1.dev.extensions` | `agentCapabilities._meta` | Enabled extension names, once each; includes `workflow_update` only when requested, since #680. | #673 |

## Extensions

### `workflow_update`

- **Capability:** `workflow_update`, under extension version `1`. Opt in at initialization:

```json
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"_meta":{"p1.dev":{"version":1,"capabilities":["workflow_update"]}}}}}
```

The answer's `agentCapabilities._meta["p1.dev"]` is `{"version":1,"extensions":["workflow_update"]}`. This is per connection and passed to every session process by the router.

- **Wire shape:** agent → client notification `_p1/workflow_update`, params `{sessionId,event}`. No request id or answer. `event.type` selects one of the eight observations below; fields are camelCase. Optional fields are emitted as `null` when unknown or absent (decoders may also accept omission). Counts, attempts and ordinals are non-negative integers; ordinals start at 1. No timestamps or inferred totals are added. The transport order is the observation order; notifications already queued by a turn precede its prompt answer on the same writer.

```json
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"run_started","id":"wf7","resumedFrom":"wf2"}}}
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"phase","run":"wf7","name":"Review"}}}
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"log","run":"wf7","text":"checking\nsecond line"}}}
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"jobs_queued","run":"wf7","count":3}}}
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"step_started","run":"wf7","ordinal":2,"call":"call-a","label":null,"phase":"Review","role":"reviewer","model":"env/deep:high","workerId":"w9","attempt":4,"prompt":"public script task"}}}
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"step_ended","run":"wf7","ordinal":2,"call":"call-a","label":"Inspect","model":"env/deep:high","status":"blocked","attempts":4,"replayed":false,"error":"needs checklist","workerId":"w9"}}}
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"thunk_failed","run":"wf7","error":"invalid item"}}}
{"jsonrpc":"2.0","method":"_p1/workflow_update","params":{"sessionId":"sess1","event":{"type":"run_ended","id":"wf7","outcome":"completed_with_issues","error":null}}}
```

`run_started` opens a run; `resumedFrom` names the prior run whose journal is replayed. `phase` changes its current phase; `log` is script text, and `jobs_queued` reports one parallel/pipeline fan-out, not an advance list of tasks. `thunk_failed` is a run-level failure note, not a step. `run_ended.outcome` is `completed`, `completed_with_issues`, `failed` or `cancelled`; `error` is the run's optional diagnostic.

Steps are keyed by **run + ordinal**, never call or worker id. Repeated `step_started` observations update that row (the early worker announcement and later engine observation, or a fallback's new worker), not a second step. `attempt` is the current attempt; `model` is `environment/profile[:effort]`; `prompt` is the script's task text, **not a worker's assembled prompt**. A `step_ended` may arrive without a start (replay or refusal), so it also carries label/model; `replayed` states whether its result came from the journal. `status` is `done`, `failed`, `blocked` or `cancelled`; `attempts` is the attempts performed. Missing worker, label, phase, resume source or error stays unknown/absent, not a fabricated value. The frozen synthetic eight-callback seam replay is [`fixtures/workflow-run-p1dev.jsonl`](fixtures/workflow-run-p1dev.jsonl); standard real-host workflow fixtures remain unchanged.

- **Gate:** requires a valid `p1.dev` version `1` declaration **and** the exact `workflow_update` name in its `capabilities`. No declaration, empty list, unknown names, malformed list or unsupported version sends **no** `_p1/workflow_update`. Standard workflow cards, flat plan, permissions and hold behavior remain unchanged for both clients. This is a live event stream, not a reload/resume snapshot; no tree persistence across restarts is promised.
- **Standard successor:** candidate only: the [Subagent Sessions RFD](https://agentclientprotocol.com/rfds/subagents.md), still a draft behind `unstable_subagents` when checked 2026-10-10, uses `subagent_update` for worker associations and current work state. Stable ACP `plan` already supplies the flat view. **Neither is a full tree equivalent:** they do not encode workflow phases, queued jobs, replayed steps, attempts, thunk failures or run outcomes. The RFD associates reusable worker conversations, not workflow-step lifetimes. p1 does not advertise or implement the draft here.
- **Retirement:** when an ecosystem-agreed successor stabilises and an equivalent mapping exists, for a client advertising that successor p1 sends both forms for one release, then drops `_p1/workflow_update` for that client. Stabilisation of worker associations alone cannot retire still-unrepresented workflow tree data; those semantics need a standard equivalent first. Clients without the successor keep the negotiated extension until they can migrate. TCK **NOT RUN**; eight-event serde round trips, negotiated and silent seam replays, and the existing standard fixtures provide this slice's evidence.

## Section template for an extension

Every later extension adds one section in this shape:

### `<name>`

- **Capability:** the string a client lists in `clientCapabilities._meta["p1.dev"].capabilities`, and the one p1 lists in `extensions` when it enables it.
- **Wire shape:** each method, notification or `_meta` key, with a complete example JSON line per direction.
- **Gate:** what a non-declaring client receives instead. Usually nothing, or the standard form.
- **Standard successor:** the ACP shape, RFD or draft that will replace it, with a link, or "none known".
- **Retirement:** the condition for dropping the `_p1` form. Both forms are sent for one release after the successor stabilises.
