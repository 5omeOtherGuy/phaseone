# p1 over ACP: the published surface

This document is the complete contract between `p1 acp` and its clients. It covers the standard ACP v1 surface p1 implements and every p1-specific extension, now and to come. Anyone must be able to build a client for every p1 capability from this document and the fixtures in [`fixtures/`](fixtures/), without reading p1's Rust (ADR-0154). Its layout follows JetBrains' `air-extensions.md` in [zed-industries/claude-code-acp](https://github.com/zed-industries/claude-code-acp).

## Purpose and scope

p1 implements the agent side of ACP v1 over stdio. Standard ACP comes first, and an ecosystem-agreed shape is preferred over a p1-specific one. A p1 feature with no standard form becomes an extension:

- a `_p1/...` method or notification, or
- a key under `_meta["p1.dev"]`.

Each extension is sent only to a client that declared it.

The extensions are `workflow_update`, `worker_transcripts`, `run_cancel`, `worker_stop` and `agent_state`. A valid declaration with an empty capability list still gets an empty extension list; a client must name each extension it wants.

## Compatibility rule

- A client that does not declare `p1.dev` receives standard ACP v1 and ecosystem-agreed shapes below: no `_p1/...` message and no `p1.dev` key. One test per extension proves this, and `crates/p1-acp/tests/driver.rs` proves it for the whole first slice.
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
  "_meta":{"steering":{"supported":true}},
  "agentCapabilities":{"loadSession":true,
    "promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},
    "sessionCapabilities":{"close":{},"list":{},"resume":{},"fork":{}},
    "_meta":{"p1.dev":{"version":1,"extensions":[]}}}}}
```

An unsupported `protocolVersion` gets p1's latest supported version, 1, and the client decides whether to continue.

## Ecosystem keys

### `_session/steering`

This is the [Claude Code ACP adapter's agreed steering wire](https://github.com/zed-industries/claude-code-acp/blob/main/src/acp-agent.ts), not a `p1.dev` extension. Every client's `initialize` response advertises top-level `_meta.steering = {"supported":true}`, beside `agentCapabilities`, regardless of its declaration.

```json
{"jsonrpc":"2.0","id":3,"method":"_session/steering","params":{"sessionId":"<session>","prompt":[{"type":"text","text":"use the revised plan"}],"_meta":{"steering":{"idleBehavior":"promptRequired"}}}}
{"jsonrpc":"2.0","id":3,"result":{"outcome":"injected"}}
```

- `prompt` takes the text and resource-link blocks of `session/prompt`, not its images (#694); an empty array, unsupported content or an unknown session is invalid params (`-32602`). No priority is exposed. An unsupported `idleBehavior` is also invalid params.
- While a prompt is running **or held**, input enters the existing operator inbox at the next model boundary and answers `{"outcome":"injected"}`. It does not cancel the turn, dispatch slash commands, or release the hold.
- Idle, the default answers `{"outcome":"startedNewTurn"}` immediately and starts an ordinary detached prompt turn. Its updates stream normally, with no second response or `stopReason` for this request. Cancel and the hold rule apply as usual; a turn failure is reported on stderr.
- With `_meta.steering.idleBehavior: "promptRequired"`, idle answers `{"outcome":"promptRequired","reason":"noRunningTurn"}` without queuing or running anything. The client can submit the content through its next `session/prompt`. During a running turn this option still injects. A host command such as `/compact` is not a model turn; it takes the idle behavior, while a skill command that became a model turn accepts injection.
- **Standard successor:** the ACP standard steering method when published; expect the ecosystem method to be renamed then. Pin today's shape until that successor stabilises. Fixture: [`fixtures/steering.jsonl`](fixtures/steering.jsonl). TCK NOT RUN / real client NOT RUN for this slice; scripted real-host replay is the evidence.

## Client MCP servers

The `mcpServers` of `session/new`, `session/load`, `session/resume` and `session/fork` become that session's MCP servers (#695, ADR-0182), next to the environment's `[[mcp_servers]]` (ADR-0181), under the same rules. Source: `p1-acp/src/wire/v1/mcp.rs` decodes them, `p1-host/src/mcp.rs` connects them.

| Server | Shape | What p1 does |
| --- | --- | --- |
| stdio | `{name, command, args?, env?: [{name, value}]}`, no `type` or `"stdio"` | Starts `command` with `args` in the session's folder. The child gets the non-secret baseline p1's shell keeps (`PATH`, `HOME`, `LANG`, `LC_*`, …) plus exactly the listed variables; no other variable of p1's environment. |
| http | `{type: "http", name, url, headers?: [{name, value}]}` | Streamable HTTP, `http` or `https`, with the listed headers (an `Authorization` header is fine); a header the transport sets itself (`Content-Type`, `Mcp-Session-Id`, …) is refused. |
| sse | `{type: "sse", …}` | Refused: invalid params (`-32602`). `mcpCapabilities.sse` is `false`. |

- Tools are named `mcp__<server>__<tool>` and join the main agent's assembly before its first turn, with approval, masking and the journal as for any tool. Workers get no MCP tools.
- A server name follows the environment's rule (lowercase letters, digits, `-` and `_`, at most 32, no `__`). A name the environment or another client server already uses, a tool-name clash, or a server that does not connect fails the request with `-32603` and a message that starts `p1 could not start the session:` and names the server. A malformed entry is invalid params (`-32602`) naming the entry.
- Every `env` and header value is treated as a credential: masked in tool results, the journal, stderr and the wire, and never journalled; the journal names the servers and tools only.
- A `session/load` or `session/resume` that re-attaches to a session open on the connection keeps the servers the session opened with.

## The standard surface p1 implements

| Method or update kind | Direction | p1 source | Notes |
| --- | --- | --- | --- |
| `initialize` | client → agent | `p1-acp/src/router.rs`, `capabilities.rs`, `wire/v1` | v1 only. `loadSession`, `sessionCapabilities.close/list/resume/fork`, `mcpCapabilities` `{"http": true, "sse": false}` (stdio servers are the baseline; ADR-0182). `promptCapabilities.image` is true when the model the run starts with takes image input on its route (see "Image prompts"); no audio or embedded context, no auth methods. |
| `session/new` | client → agent | `router.rs` | Any number per process, each with its own id. `cwd` is the session's folder and must be an existing absolute directory, else invalid params (`-32602`). Without `cwd`: `p1 acp`'s `--workspace`, else invalid params. `mcpServers` become the session's MCP servers (see "Client MCP servers"). The answer carries `configOptions` (below) and `modes` (the permission modes, below). |
| `session/load` | client → agent | `router.rs`, `driver/session.rs`, `replay.rs`, host journal archive | `sessionId`, `cwd`, `mcpServers` (as for `session/new`); reopen a saved journal under its writer lock, replay user/assistant chunks (a user image as image content) and tool cards, then the command list, before answering with settings (no `sessionId`); nothing follows the answer (ACP-LOAD-002). A session still open on the connection re-attaches instead (see "Durable sessions"). |
| `session/resume` | client → agent | same | Restore the same session and settings without transcript replay; the command list precedes the response, which has no `sessionId`. A session still open on the connection re-attaches. |
| `session/list` | client → agent | `router.rs`, neutral `SessionArchive` | Optional `cwd` and opaque `cursor`; `{sessions:[{sessionId,cwd}],nextCursor?}`. Canonical cwd filtering, newest-created id first, at most 100 per page. No title or timestamp invented. |
| `session/fork` | client → agent | same | `sessionId`, `cwd`, optional `mcpServers` (as for `session/new`); validated journal snapshot into a new id, opened for continuation; returns `sessionId` and settings. ACP v1's fork schema is **unstable** (the TypeScript method is `unstable_forkSession`), but the wire name is `session/fork`, not the TS alias. |
| `session/set_config_option` | client → agent | `p1-acp/src/config_options.rs`, `driver/session.rs`, `p1-host/src/frontend_port/config.rs` | Runs the line mode's `/model` (`configId` `model`) or `/effort` (`thought_level`) switch, or changes the permission mode (`mode`), and answers `{"configOptions":[...]}`, the complete list. Between turns the switch runs at once; a switch that fails is an internal error (`-32603`) and changes nothing. During a prompt the change waits, the answer shows the list as the next turn will run it, and the switch runs before that turn; a waiting change that then fails is reported on p1's stderr, and the update shows the unchanged value. A waiting model switch replaces a waiting effort and removes `thought_level` from the list until it ran. An unknown `configId` or a value the option does not list is invalid params (`-32602`). |
| `session/update` `config_option_update` | agent → client | `driver/session.rs` | The complete list, after every switch that ran, sent after the answer of the request that caused it. |
| `session/set_mode` | client → agent | `driver/session.rs`, `p1-host/src/frontend_port/mode.rs` | The same mode change as `set_config_option` `mode`; `modeId` is one of the `availableModes`. Answers `{}`, then sends `config_option_update` and `current_mode_update`. It applies at once, also during a prompt: the next tool call is decided by the new mode. An unknown or missing `modeId`, or a session without modes, is invalid params (`-32602`). |
| `session/update` `current_mode_update` | agent → client | `driver/session.rs` | The new `currentModeId`, after every mode change, by either method. |
| `session/close` | client → agent | `router.rs` | Ends one session as `session/cancel` would, stops its workflow runs and workers, and answers `{}` once the session is gone. Its pending prompt answers `cancelled`. Its id is unavailable for prompts (`-32602`) until load/resume; the journal is retained. |
| `session/prompt` | client → agent | `driver/session.rs`, `p1-host/src/frontend_port/session.rs` | Text, `image` and `resource_link` blocks. A link reaches the model as `[name](uri)`. An image is stored and sent by reference (see "Image prompts"); one the session's model cannot take, or a malformed one, gets invalid params. `audio` and `resource` blocks get invalid params. The answer is the stop reason below, under the hold rule. A prompt that starts with `/` and an advertised command name runs that command instead (see "Slash commands"). |
| `session/cancel` | client → agent | `driver/session.rs` | Cancels the turn and every running workflow run and worker, and releases a held prompt. A parked permission request resolves as deny, and the prompt answers `cancelled`. |
| `session/update` `agent_message_chunk` | agent → client | `p1-acp/src/sink.rs` | Assistant text deltas. |
| `session/update` `agent_thought_chunk` | agent → client | `sink.rs` | Reasoning deltas. |
| `session/update` `tool_call` | agent → client | `sink.rs`, `driver/session.rs` | `pending` when the call needs permission, sent before its permission request. `in_progress` when a call starts unannounced. |
| `session/update` `tool_call_update` | agent → client | `sink.rs` | `in_progress` when an announced call is permitted. `completed` or `failed` with the result text. |
| Workflow and worker `tool_call_update` content | agent → client | `workflow_card.rs`, `frontend_port/workflow.rs` | A workflow's initiating card stays `in_progress` until its run ends; cumulative progress lines replace content. Worker end notes append to the initiating card without changing its status, else become assistant text. |
| `session/update` `usage_update` | agent → client | `usage.rs`, `sink.rs` | After each parent response with known input usage and effective context capacity. Latest input-plus-cache tokens as `used`; capacity as `size`; optional cumulative USD cost. |
| `session/update` `available_commands_update` | agent → client | `p1-acp/src/commands.rs`, `driver/session.rs`, `p1-host/src/frontend_port/commands.rs` | The session's slash commands, right after the `session/new` (and `session/fork`) answer, right before the `session/load` and `session/resume` answer, and again when a switch or a reload changed them. See "Slash commands" below. |
| `session/update` `plan` | agent → client | `plan.rs`, `sink.rs`, `frontend_port/` | Complete replacement of the agent-authored `todo_write` list after durable commit; observed workflow steps are the fallback until the first authored list. |
| `session/update` `session_info_update` | agent → client | `session_title.rs`, `driver/session.rs`, neutral `SessionHandle::title` | Title before the first prompt answer, then before an answer if the title changed (after it for a client that opted out of the hold). Standard metadata, no negotiation. |
| `session/request_permission` | agent → client | `p1-acp/src/policy.rs` | Options `allow_once`, `allow_always`, `reject_once`. A `cancelled` outcome or an unknown option id denies. Asked for every tool call in the `ask` mode, never in the others. |
| `elicitation/create` | agent → client | `p1-acp/src/questions.rs`, `p1-host/src/frontend_port/questions.rs` | Session-scoped form questions, only with `clientCapabilities.elicitation.form: {}`. Choice enums and free text; see below. |

### Durable sessions

ADR-0158: the host composes the neutral `SessionArchive` port and its JSONL catalog adapter; ACP owns wire translation only. **ASSUMPTION while #62 is open:** journals use `P1_STATE_DIR` (the p1 state root), else `$XDG_STATE_HOME/p1`, else `$HOME/.local/state/p1`, under `sessions/acp/<id>/`. Each directory is private (0700 on Unix); journal and workspace metadata are 0600. IDs persist across router restarts and cannot name arbitrary file paths. A saved session must be reopened in its original canonical cwd. `--session` and `--resume` remain invalid on the router command, not on its internal child processes.

Load and resume launch the existing session child with `--session FILE --resume`. Existing route/model compatibility checks and crash reconciliation apply, with no old call dispatched. Already-open or externally locked journals cannot get a second writer.

Load or resume of a session still open on the same connection (ACP TCK ACP-LOAD-001..003, ACP-RESUME-001/002) re-attaches to its running process (ADR-0156, ADR-0178): no second process or journal writer, the same agent, settings, workers and workflow runs. The id must belong to that cwd in the catalog, as for a closed session. Load replays the journal's display transcript, the snapshot the process resumed from plus every record it committed since (the front-end port's `record_committed` hook), before the answer; resume replays nothing. Both publish the command list again and then answer the settings without `sessionId`. Inbox turns then wait for the next prompt, as after any load. A load during a running prompt replays what the journal holds so far; the prompt keeps streaming. Close stops the process but retains the journal. Fork copies the bounded, validated append-only snapshot, assembly identities, format version and record times; a torn tail is excluded without repairing the parent. A live parent may continue independently. Fork does not clone running workers/workflows, tool-output stores or scratch files; those are process resources, not conversation history.

Load replays the full recorded display transcript, including turns hidden by compaction, not the compacted model projection. User input becomes `user_message_chunk`, each text block of input with images one chunk and each image an image chunk with the stored bytes (an image the store cannot read: its placeholder text), assistant text `agent_message_chunk`, visible reasoning `agent_thought_chunk`, calls `tool_call`, and results `tool_call_update` with success/failure. Tool names/raw inputs/results are preserved; historic tool categories fall back to `other` because the journal has no display descriptor. Environment/system prompts and opaque provider replay payloads never go to the client. The model resumes through the existing projection. Fixture: [`fixtures/session-load.jsonl`](fixtures/session-load.jsonl), from [`fixtures/session-journal.jsonl`](fixtures/session-journal.jsonl), recorded on a scripted provider, not real user data.

TCK **NOT RUN** and Martty/acpx real client **NOT RUN** for this slice; offline journal fixture and router/archive tests are the evidence.

### Image prompts

A `session/prompt` may carry standard ACP `image` blocks (#694, ADR-0183): `{"type":"image","mimeType":"image/png","data":"<base64>"}`, PNG, JPEG, GIF or WebP; `uri` is ignored. Sources: `driver/session.rs` maps them, `p1-host/src/frontend_port/session.rs` stores them, `replay.rs` replays them.

- **Advertised:** `initialize` answers `promptCapabilities.image: true` when the model `p1 acp`'s run options select takes image input on its route: the profile lists `image` in `input_modalities` and the route's adapter sends images (today `anthropic-messages`; the OpenAI adapters send a text placeholder, ADR-0180). The shipped Claude profiles take images. The router answers `initialize` before any session builds its provider, so the flag is decided once, at start.
- **Checked per prompt:** each image is refused with invalid params (`-32602`) when the session's model does not take image input at that turn (also after `session/set_config_option` switched to such a model), when the store refuses it (not an image, above 5 MB or 8000 px a side), when `data` is not base64 or `mimeType` is not one of the four. A refused prompt runs no turn and records nothing in the journal (images of the same prompt stored before the refusal stay in the store, which never deletes); an image is never dropped silently. `_session/steering` takes no images (invalid params).
- **Stored, not journalled:** the bytes go into the session's attachment store, metadata cut losslessly (ADR-0180). The turn's journal record is `user_content` with references only; the host resolves the bytes per request. A prompt without an image is text, as before: its `user_input` record is unchanged. A prompt with an image is a model turn, never a slash command.
- **Replayed:** `session/load` sends each text block of such a prompt as a text `user_message_chunk` and each image as an image `user_message_chunk` with the stored bytes. An image the store cannot read is replayed as its placeholder text, naming why.

Fixture: [`fixtures/image-prompt.jsonl`](fixtures/image-prompt.jsonl): an image prompt, then a `session/load` that replays it, on a scripted provider whose route takes images. Real-client prompt and TCK **NOT RUN** for this slice.

### Session titles

The standard ACP v1 `session_info_update` carries `{"sessionUpdate":"session_info_update","title":"Fix the parser"}` inside a `session/update` with that session's `sessionId`. It is the turn's last update before the first `session/prompt` answer, on the same writer queue: no `session/update` of a turn follows that turn's answer (ACP-CANCEL-002), also when the prompt is cancelled. The driver reads the neutral `SessionHandle::title` as each turn ends and sends another update only if the selected title differs. A client that declared `_meta["p1.dev"]` `agent_state` (#683) takes updates after the answer by design and keeps the earlier order: the answer, then the title. Commands and cancelled or failed turns also count as prompts; later prompt text does not replace the fallback.

**ASSUMPTION (#693):** the host and journal currently have no session-title source. The default port hook returns `None`, so p1 uses the first prompt's opening words, with whitespace collapsed and at most **80 Unicode scalar values**, cutting at the last word boundary where possible. A single long word is cut at 80 characters; empty opening text becomes `Untitled session`. No ellipsis, model call, persisted title or rename command is added. A host-supplied title is reused verbatim. Titles are session-local. Fixture: [`fixtures/session-title.jsonl`](fixtures/session-title.jsonl); driver and router tests also cover source changes and isolation.

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
| `ask` (start with `--ask`) | Every call asks the client (`session/request_permission`). | always |
| `read-only` | Calls that only read run without asking; every call that writes files, executes or delegates is refused without asking: "Not permitted in read-only mode: this call writes, executes or delegates." | always |
| `full-access` (default) | Every call runs without asking, in the shell sandbox `p1 acp` started with. | only when `p1 acp` started without `--ask` |

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

`/model` and `/effort` appear only when the session has those config options. Every command but a skill reports as one `agent_message_chunk` and ends the prompt `end_turn` without a model turn; a failure is reported the same way, as text. `session/cancel` reaches a host command, which then answers `cancelled`; `/model` and `/effort` run at once and answer `end_turn`. A skill command's turn is an ordinary prompt turn. A command is a prompt like any other: it waits behind a running prompt and releases a held one (the hold rule). The list is published again after a switch or `/modules reload` that changed it: before the answer of a prompt that ran `/modules reload` or `/model` (after it for a client that opted out of the hold), after the answer of a `session/set_config_option`.

Every `session/update` and `session/request_permission` carries the `sessionId` of the session it belongs to. A request naming an unknown `sessionId` gets invalid params (`-32602`).

Workers' own activity goes to stderr unless `worker_transcripts` is negotiated; their end notes use the standard form below for every client.

### Operator questions

The standard [`elicitation/create` form mode](https://agentclientprotocol.com/rfds/elicitation.md) needs no `p1.dev` declaration. A client must explicitly advertise an object at `clientCapabilities.elicitation.form`; absent, null, empty elicitation and URL-only capabilities keep the headless question path. Questions still require a user invitation (`ask me`, `ask questions`, or the other ADR-0135 phrases); workers share that session invitation and the host serializes questions. Fixture: [`fixtures/question.jsonl`](fixtures/question.jsonl).

- One request carries the batch in a flat `requestedSchema` with `type: "object"`. For question index N (from zero), `qN` is a string with titled `oneOf` choices, or an array with titled `items.anyOf` choices for multi-select. Choice values are the original labels. Option previews append to their descriptions after a blank line; no `_meta` is needed. Headers title the fields, question text describes them, and worker questions prefix titles with `[worker-id]`.
- `qN_text` is an optional nonempty string for free text, instead of or alongside the choices. Both fields are optional because the restricted flat schema cannot express a required alternative; p1 validates that each question has choices or nonblank text. Chosen labels must be known, unique, and of the right single/multi-select shape. p1 returns them in source option order, with free text unchanged.
- An `accept` with valid `content` answers the tool. `decline`, `cancel`, unknown actions, missing or invalid answers, JSON-RPC failure and connection loss take the question's cancel path. `session/cancel` abandons pending elicitation immediately; late replies do not resume it. An already cancelled question sends no request.
- The request carries `sessionId`, `mode: "form"`, the question text as `message`, and the schema. For example, a client answers `{"action":"accept","content":{"q0":"B","q0_text":"with tests"}}`. No URL mode or secret-collection flow is implemented by this slice; form questions must not solicit credentials.

### Session usage

The stable ACP v1 [`usage_update`](https://agentclientprotocol.com/protocol/v1/prompt-turn#session-usage-updates) needs no extension declaration. [`fixtures/usage.jsonl`](fixtures/usage.jsonl) records tool-use and text responses, followed by unknown usage and recovery.

- `used` is the latest response's uncached input plus reported cache-read and cache-write tokens, not a lifetime sum. Output and reasoning tokens are not added. Cache categories a route does not report add nothing; unknown uncached input means unknown context usage.
- `size` is the parent's effective context window, including the selected profile's capacity, not its earlier summarization threshold. Unknown input usage or an unknown window sends no update.
- `cost`, when known, is cumulative parent-response spend for that session: `{"amount":0.00325,"currency":"USD"}`. p1 converts micro-USD to USD. A response with unknown usage or cost makes cumulative cost unknown for the rest of that session; subsequent updates omit `cost`, never substitute zero or publish a partial total. A reported known zero remains zero.
- Session state is independent across prompts and sessions. Workers' context and costs are excluded from parent usage; declaring clients get separate worker-tagged usage below. Usage updates are flushed before the prompt's reply, like other updates.

### Agent-authored plans and the workflow fallback

The standard ACP v1 [`plan`](https://agentclientprotocol.com/protocol/v1/agent-plan) needs no extension declaration. The `todo_write` tool replaces the complete session-owned list, with content, `pending` / `in_progress` / `completed` status and `high` / `medium` / `low` priority. Its successful canonical snapshot is stored in the normal `ToolFinished` journal record. The host projects only committed successful results of the known implementation, including renamed faces, and forwards the neutral `plan_updated` hook. Failure, denial, cancellation or an uncommitted result leaves the list unchanged. Resume restores the latest snapshot from all durable records, even after context compaction.

Replacement, not merge, is deliberate: items have no stable ids, so merging by content would leave removed or renamed tasks behind. An empty list explicitly clears it. The authored list takes precedence over inferred workflow steps from its first write onward, including after clearing; workflow cards remain live independently. This avoids duplicating work or allowing background steps to overwrite the agent's own plan (ADR-0159). [`fixtures/todo-plan.jsonl`](fixtures/todo-plan.jsonl) freezes a real component call with all statuses/priorities, then clearing, through the host and ACP driver. The prompt guidance lives with the tool. No local Iris donor checkout was available; it is an original draft, not claimed donor reuse.

Before the first authored list, the workflow engine's step start/end observations and `RunReport.steps` provide the existing fallback through the neutral front-end port:

- Each update replaces the complete plan for the session: all observed workflow steps, including earlier runs and repeated start events. Rows are keyed by run + ordinal (the run's `agent()` call order), not call or worker id. Runs keep start order; steps keep ordinal order. A fallback updates the same row. State never crosses sessions.
- `content` is `<run>/<ordinal>: <label>`, falling back to the script's task text. An end-only replay or refusal uses its label, else call id; an existing row retains its task text when the end event has none. This is not the worker's assembled prompt.
- Running steps have `status: "in_progress"`; successful `done` steps have `status: "completed"`. Failed, blocked and cancelled goals remain unfinished: they have `status: "pending"` and an explicit `(failed)`, `(blocked)` or `(cancelled)` suffix in content. **This is lossy:** ACP's three states cannot encode those terminal execution outcomes. Here `pending` means the goal remains unfinished, not a promise that p1 will retry it. They are never marked successfully completed.
- Every entry has `priority: "medium"`: workflow state has no relative priorities, so the projection gives all steps equal priority. Queued-job counts are not fabricated into pending tasks; steps appear only when observed. This is execution progress, not an advance plan.
- Step-end updates are queued before releasing a held prompt. [`fixtures/plan.jsonl`](fixtures/plan.jsonl) freezes only the plan notifications from a real-host two-step workflow (done, then blocked). Its test drives initialization, prompt and approvals; unrelated parent output and permission traffic are not frozen because they can interleave with the background workflow.
- The detailed workflow tree uses the negotiated `workflow_update` extension below. Workers' own activity uses the separate `worker_transcripts` extension when negotiated, else stderr.

### Workflow cards and worker end notes

These are standard ACP v1 `tool_call_update` notifications, with no extension declaration. [`fixtures/workflow-run.jsonl`](fixtures/workflow-run.jsonl) freezes the initiating cards from a real-host two-step workflow followed by a direct worker. The test drives two held prompts and approvals separately; unrelated background traffic is not frozen.

- A successful workflow start result links the run id to its initiating `toolCallId`. The card remains `in_progress`, even though the start tool returned. Run start, phase, log, queued-job count, step start/end, thunk failure and run-end summary append lines to this card. Phase, log and end lines retain the host's existing wording. Each content update replaces the cumulative text, not just the newest line; observations arriving before the start result are buffered and replayed in order.
- The final update marks `completed` only for outcome `completed`. `completed_with_issues`, `failed` and `cancelled` become `failed`, with the actual outcome retained in the summary. **This is lossy:** ACP's tool statuses do not distinguish those run outcomes. The final card is queued before releasing the run's hold; the plan projection and hold rule are unchanged.
- A worker end note uses the host's existing `worker_end_note` wording. If its worker id can be linked to a delegation start result, it appends to that initiating card with no `status` field: the already completed start tool is not reopened. Otherwise it becomes an `agent_message_chunk`. Early notes wait for outstanding start results before falling back to assistant text. A reused call id invalidates the old association. Card state is session-local.
- Links recognise assembled workflow/delegation implementation identities, including renamed faces and fixed delegation tools, and parse the tools' `Started workflow <id>` / `Started worker <id> on …` result prefixes. The fixture pins those real tool-result formats so a wording change cannot silently disable linking. Detailed worker activity uses `worker_transcripts`; workflow trees use `workflow_update` below.
- TCK **NOT RUN** for this slice; scripted real-host fixture replay and sink/codec tests are its conformance evidence.

## The hold rule

While a `session/prompt` is pending, a workflow run or delegated worker that starts belongs to it. The prompt stays pending while such work is live. When the work ends, the parent's inbox turn about it runs inside the same prompt, and its updates stream as usual. A background worker reports its end before its notice reaches the inbox, so the prompt also waits for that notice. The prompt answers when the last of these turns ends.

`session/cancel` releases the hold, and so does the next `session/prompt`. The held prompt answers first, then the next one runs. A background shell job never holds.

A notice that reaches the inbox after the hold ended is delivered in the next prompt's turn. A client that declares [`agent_state`](#agent_state) opts out of this rule; it is the only behaviour that differs by capability.

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
| `p1.dev.capabilities` | `clientCapabilities._meta` | Requested extension names; `workflow_update` opts into the tree (#680), `worker_transcripts` into child transcripts (#681), `run_cancel` and `worker_stop` into targeted cancellation (#682), `agent_state` out of the hold rule (#683). Unknown names are ignored. | #673 |
| `p1.dev.extensions` | `agentCapabilities._meta` | Enabled extension names, once each; each name appears only when requested. | #673 |
| `p1.dev.worker` | `session/update` **update object's** `_meta` | `{id, route, model}` identifies the worker and its selected provider route/wire model; only with negotiated `worker_transcripts`. | #681 |
| `worker_transcripts` | `p1.dev.capabilities` / `p1.dev.extensions` | Opt-in / enabled name for worker chunks, tools, usage and `_p1/worker_update`. | #681 |

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

### `run_cancel` and `worker_stop`

- **Capability:** independent names `run_cancel` and `worker_stop`, under version `1`. Declaring one never enables the other. Both are enabled by:

```json
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"_meta":{"p1.dev":{"version":1,"capabilities":["run_cancel","worker_stop"]}}}}}
```

The answer's `agentCapabilities._meta["p1.dev"]` is `{"version":1,"extensions":["run_cancel","worker_stop"]}` (plus any other requested, supported extensions). The router passes the declaration to every session process.

- **Wire shape:** client → agent requests `_p1/run/cancel {sessionId,runId}` and `_p1/worker/stop {sessionId,workerId}`. All ids are strings, using the host ids from workflow updates and worker associations, scoped to that session. Agent → client answers with `{"cancelled":bool}` or `{"stopped":bool}`: `true` if the target was observed running and the service accepted cancellation, `false` for an unknown or already-ended target, an unavailable service, or a service refusal. Completion racing with cancellation may still answer `true`: this is cancellation acceptance, not a promise of a particular terminal outcome.

```json
{"jsonrpc":"2.0","id":7,"method":"_p1/run/cancel","params":{"sessionId":"sess1","runId":"wf7"}}
{"jsonrpc":"2.0","id":7,"result":{"cancelled":true}}
{"jsonrpc":"2.0","id":8,"method":"_p1/worker/stop","params":{"sessionId":"sess1","workerId":"w9"}}
{"jsonrpc":"2.0","id":8,"result":{"stopped":true}}
{"jsonrpc":"2.0","id":9,"method":"_p1/run/cancel","params":{"sessionId":"sess1","runId":"unknown"}}
{"jsonrpc":"2.0","id":9,"result":{"cancelled":false}}
```

The host calls the existing service canceller for exactly that id, without taking the parent agent lock. Cancelling a workflow ends its own step workers as usual, not unrelated work. Neither request cancels the parent turn, releases its prompt hold, nor stops sibling runs or workers. Background end signals and inbox notices still determine when a held prompt can finish. Replies go through the same writer queue as standard traffic. Unknown `sessionId`, missing ids or non-string ids get invalid params (`-32602`). An empty string is an unknown id, not a broadcast. Fixture: [`fixtures/run-cancel-p1dev.jsonl`](fixtures/run-cancel-p1dev.jsonl) records real-host unknown-id replies; channel tests exercise accepted and refused hooks through both driver and router during a parent turn, and the real-host port test proves a workflow cancellation leaves a direct worker running.

- **Gate:** requires a valid `p1.dev` version `1` declaration and the method's exact capability name. Otherwise the request answers method-not-found (`-32601`), before session or target validation, without calling a hook. No new notifications are introduced. Standard `session/cancel` remains available to every client and cancels the parent turn and all background runs/workers.
- **Standard successor:** `session/cancel(childId)` from the [Subagent Sessions RFD](https://agentclientprotocol.com/rfds/subagents.md), still a draft and the planned primary form per #682's lead comment. The ecosystem `_session/async_task/stop` described by [Claude Code ACP / AIR](https://github.com/zed-industries/claude-code-acp/blob/main/docs/air-extensions.md) is also a candidate for background runs. Mapping host run/worker ids to successor targets belongs to that successor, not this fallback; this slice does not advertise draft subagent capabilities.
- **Retirement:** when either successor is published and stable, and an equivalent targeting mapping exists, clients advertising that successor get both forms for one release, then the corresponding `_p1` method is dropped for those clients. A successor covering runs alone does not retire worker stop. Clients without the successor keep the negotiated fallback until they migrate. TCK **NOT RUN / real client NOT RUN** for this slice.

### `worker_transcripts`

- **Capability:** `worker_transcripts`, under `p1.dev` version `1`. Initialization accepts `{"_meta":{"p1.dev":{"version":1,"capabilities":["worker_transcripts"]}}}` in `clientCapabilities` and answers with `{"version":1,"extensions":["worker_transcripts"]}` under `agentCapabilities._meta["p1.dev"]`. Independent of `workflow_update`; duplicates enable it once. The router passes negotiation to every session process.
- **Wire shape:** standard `session/update` kinds (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `usage_update`) carry `_meta["p1.dev"].worker = {id, route, model}` on the **update**, not on params. `sessionId` remains the parent's session. Route and model are the provider route and wire model selected at child assembly, not an assembled prompt or profile reference. Tool ids belong to the worker: clients key them by parent session + worker id + tool call id, not call id alone. Worker state and usage are isolated from the parent and other workers. Usage follows the standard numerator/unknown-cost rules above, with that worker's configured window and cumulative cost across continued turns; it is never added to parent usage. **Known limitation (review P2):** the host's in-place direct-worker model fallback does not refresh the child sink or context callback, so the stamp and window remain the initial selection after that fallback. Workflow fallback creates a new worker/sink and is not affected.

```json
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"child answer"},"_meta":{"p1.dev":{"worker":{"id":"w9","route":"fixture-route","model":"fixture-model"}}}}}}
{"jsonrpc":"2.0","method":"_p1/worker_update","params":{"sessionId":"sess1","worker":"w9","state":"started","description":null,"note":null}}
{"jsonrpc":"2.0","method":"_p1/worker_update","params":{"sessionId":"sess1","worker":"w9","state":"ended","description":"inspect","note":"worker w9 (inspect; read) ended without finish"}}
```

`_p1/worker_update` is agent → client notification, without an id or answer. Start precedes child activity; end precedes the background end releasing a held prompt. A continued worker starts again with the same id and stamp. Starts have unknown `description`/`note` (`null`); ends use the host's existing description and `worker_end_note` when available. Workflow step ends, dropped child agents and shutdown can end without a summary; unknown fields remain `null`, never inferred success. Worker turn outcomes do not answer the parent prompt. All worker updates share the parent's sequence and the single writer queue. The standard end note/card (#678) remains available to both clients.

- **Gate:** only a valid version `1` declaration naming `worker_transcripts` receives these chunks, stamps or lifecycle updates. No declaration, empty/other capability list, malformed declaration or unsupported version sends **none**. Those clients keep worker tool/turn stderr lines and standard worker end notes, with no child transcript mixed into parent messages. Permissions and hold behavior are unchanged.
- **Standard successor:** the [Subagent Sessions RFD](https://agentclientprotocol.com/rfds/subagents.md): child `sessionId`, `subagent_update`, and child cancellation, gated on `clientCapabilities.subagents`. The planned primary form for clients declaring `subagents` is that draft; this `_meta` form is the fallback. This slice implements only the fallback and does not advertise draft support.
- **Retirement:** once the successor stabilises and its mapping lands, clients advertising it receive both forms for one release, then no `_meta` worker tagging or `_p1/worker_update`. Clients without the successor retain this negotiated fallback until they migrate. Fixture [`fixtures/worker-transcript-p1dev.jsonl`](fixtures/worker-transcript-p1dev.jsonl) is a literal host-seam replay of chunks, tools, usage, continuation and dropped-child lifecycle. The real-host workflow fixture also runs with and without negotiation, checking workflow and direct worker stamps, lifecycle and pre-answer order while keeping standard cards identical. **TCK NOT RUN / real client NOT RUN** for this slice.

### `agent_state`

- **Capability:** `agent_state`, under `p1.dev` version `1`:

```json
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"_meta":{"p1.dev":{"version":1,"capabilities":["agent_state"]}}}}}
```

The answer's `agentCapabilities._meta["p1.dev"]` is `{"version":1,"extensions":["agent_state"]}` (plus any other requested, supported extensions). The router passes the declaration to every session process.

- **Behaviour:** this is the **only behaviour that differs by capability**; every other extension adds messages and changes nothing a standard client sees. A declaring client opts out of [the hold rule](#the-hold-rule):
  - `session/prompt` answers as soon as the prompt's own turn ends, even while a workflow run or worker it started is live.
  - The run's updates keep streaming as `session/update` notifications after the answer (standard `plan` and tool-call cards, and the `_p1/workflow_update` or worker updates the client also declared).
  - The session's metadata, `session_info_update` and a command list a reload changed, follows the answer, where a standard client gets it before (ACP-CANCEL-002). So does the command list of a `session/load` or `session/resume`, which a standard client gets before that answer (ACP-LOAD-002).
  - When background work leaves a notice in the parent's inbox while no prompt is running, p1 runs the parent's inbox turn at once, as the line mode does. Its updates stream like a prompt's; it has no request and no answer.
  - A `session/prompt` that arrives during such a turn waits for it, then runs. `_session/steering` during it answers `injected`. `session/cancel` cancels it, together with every run and worker, exactly as during a prompt.
  - Inbox turns start only after the client's input. After `session/cancel`, the notices of the stopped work wait for the next `session/prompt` or `_session/steering`, as they do under the hold. A loaded or resumed session's notices about the earlier process likewise wait for the first prompt, and so do a re-attached session's (load or resume of a session still open on the connection).
- **Wire shape:** agent → client notification `_p1/agent_state`, params `{sessionId, state, stopReason?}`, sent when the state changes. No request id or answer.
  - `running`: a turn of the session runs, a prompt's or an inbox turn.
  - `requires_action`: a `session/request_permission` or `elicitation/create` request waits for the client, from the parent or a worker. When it is answered or withdrawn, the state goes back to `running`, or to `idle` if no turn runs.
  - `idle`: no turn runs and nothing waits for the client. Background work alone does not make the session `running`.
  - `stopReason` is present only on the `idle` that ends a turn: the ACP stop reason (`end_turn`, `max_tokens`, `refusal`, `cancelled`), or `error` for a turn that failed. When a worker's request still waits for the client as the turn ends, the state stays `requires_action` and the `idle` after it carries the turn's `stopReason`.

  The `idle` that ends a prompt's turn precedes the prompt's answer on the single writer queue (unless a worker's request still waits, as above), also when the prompt is cancelled while its permission request waits; every update of a turn precedes its `idle`. A prompt's run then streams after the answer, and its inbox turn has two transitions:

```json
{"jsonrpc":"2.0","method":"_p1/agent_state","params":{"sessionId":"sess1","state":"running"}}
{"jsonrpc":"2.0","method":"_p1/agent_state","params":{"sessionId":"sess1","state":"idle","stopReason":"end_turn"}}
{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess1","update":{"sessionUpdate":"plan","entries":[{"content":"wf1/1: Prepare","priority":"medium","status":"in_progress"}]}}}
{"jsonrpc":"2.0","method":"_p1/agent_state","params":{"sessionId":"sess1","state":"running"}}
{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"the run finished"}}}}
{"jsonrpc":"2.0","method":"_p1/agent_state","params":{"sessionId":"sess1","state":"idle","stopReason":"end_turn"}}
```

A permission request inside a turn is preceded by `requires_action`, after its `tool_call`; once the client answers it, `running` precedes the turn's next update. A question (`elicitation/create`) sends `requires_action` when it is asked, which may be before updates of the turn still on their way to the writer:

```json
{"jsonrpc":"2.0","method":"_p1/agent_state","params":{"sessionId":"sess1","state":"requires_action"}}
{"jsonrpc":"2.0","method":"_p1/agent_state","params":{"sessionId":"sess1","state":"running"}}
```

  Fixture: [`fixtures/agent-state-p1dev.jsonl`](fixtures/agent-state-p1dev.jsonl), a literal replay of a fake two-step workflow run through the host's port bridge (`crates/p1-host/tests/acp_agent_state.rs`, which also runs a real two-step workflow on the real host).
- **Gate:** requires a valid `p1.dev` version `1` declaration naming `agent_state`. Every other client, including a `p1.dev` client that did not name it, keeps the hold rule and receives no `_p1/agent_state`.
- **Standard successor:** ACP v2's turn lifecycle: `session/prompt` answers at once and the `state_update` session update reports `running`, `idle` (with `stopReason`) and `requires_action` ([v2 migration guide](https://agentclientprotocol.com/protocol/v2/migration.md), a draft when checked 2026-10-10). The states, their meaning and `stopReason` are taken from it. v2 leaves agent-initiated turns, such as this inbox turn, as future work. p1 does not speak v2.
- **Retirement:** when p1 negotiates ACP v2 with a client, that client gets `state_update` and never `_p1/agent_state`. A v1 client keeps this extension until v2 is stable and the client can migrate. TCK **NOT RUN / real client NOT RUN** for this slice.

## Section template for an extension

Every later extension adds one section in this shape:

### `<name>`

- **Capability:** the string a client lists in `clientCapabilities._meta["p1.dev"].capabilities`, and the one p1 lists in `extensions` when it enables it.
- **Wire shape:** each method, notification or `_meta` key, with a complete example JSON line per direction.
- **Gate:** what a non-declaring client receives instead. Usually nothing, or the standard form.
- **Standard successor:** the ACP shape, RFD or draft that will replace it, with a link, or "none known".
- **Retirement:** the condition for dropping the `_p1` form. Both forms are sent for one release after the successor stabilises.
