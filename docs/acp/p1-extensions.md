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
| `session/prompt` | client → agent | `driver/session.rs` | Text and `resource_link` blocks only. A link reaches the model as `[name](uri)`. `image`, `audio` and `resource` blocks get invalid params. The answer is the stop reason below, under the hold rule. A prompt that starts with `/` and an advertised command name runs that command instead (see "Slash commands"). |
| `session/cancel` | client → agent | `driver/session.rs` | Cancels the turn and every running workflow run and worker, and releases a held prompt. A parked permission request resolves as deny, and the prompt answers `cancelled`. |
| `session/update` `agent_message_chunk` | agent → client | `p1-acp/src/sink.rs` | Assistant text deltas. |
| `session/update` `agent_thought_chunk` | agent → client | `sink.rs` | Reasoning deltas. |
| `session/update` `tool_call` | agent → client | `sink.rs`, `driver/session.rs` | `pending` when the call needs permission, sent before its permission request. `in_progress` when a call starts unannounced. |
| `session/update` `tool_call_update` | agent → client | `sink.rs` | `in_progress` when an announced call is permitted. `completed` or `failed` with the result text. |
| `session/update` `usage_update` | agent → client | `usage.rs`, `sink.rs` | After each parent response with known input usage and effective context capacity. Latest input-plus-cache tokens as `used`; capacity as `size`; optional cumulative USD cost. |
| `session/update` `available_commands_update` | agent → client | `p1-acp/src/commands.rs`, `driver/session.rs`, `p1-host/src/frontend_port/commands.rs` | The session's slash commands, right after the `session/new` answer, and again when a switch changed them. See "Slash commands" below. |
| `session/request_permission` | agent → client | `p1-acp/src/policy.rs` | Options `allow_once`, `allow_always`, `reject_once`. A `cancelled` outcome or an unknown option id denies. Every tool call asks in this slice. |

### Config options

Each option is an ACP `select` (p1 never sends a boolean option), one per category:

- `model` (category `model`): the models in the run's scope (`--models`, else `enabled_models` in `settings.toml`, else every model the environments bind), as `ENV/PROFILE`, plus the running one. The description names the route and the efforts.
- `thought_level` (category `thought_level`): the efforts the running model's profile lists. While the session runs the profile's own setting and the profile names no default effort, a `default` value is listed as current.

A session whose environment names no profile, or whose model the environments no longer list, gets no options. Fixture: [`fixtures/model-switch.jsonl`](fixtures/model-switch.jsonl). Permission modes (#696) will be a further option in the same list.

### Slash commands

The [ACP slash commands](https://agentclientprotocol.com/protocol/v1/slash-commands): the list is closed and built by the host (`SessionHandle::commands`). A prompt runs a command when its text is `/NAME`, alone or followed by whitespace and the argument, and `NAME` is in the last list. Any other text, an unknown `/x` included, is a prompt for the model. Fixture: [`fixtures/slash-commands.jsonl`](fixtures/slash-commands.jsonl).

| Command | Input hint | What it does |
| --- | --- | --- |
| `/model` | `ENV/PROFILE` | Without an argument, the model option's values as text, the current one marked `*`. With one, the same switch as `session/set_config_option` `model`: a `config_option_update`, then `model: E/P` or `model not changed: <reason>`. |
| `/effort` | `level` | The same for `thought_level`. |
| `/compact` | — | The line mode's `/compact` (ADR-0076): one summary of the history now. Reports `compacted: A → B tokens`, `nothing to compact`, or `compact failed: <reason>` (an environment without `[context]` has no summarizer). |
| `/status` | — | Environment, model, route, effort, access, sandbox and workspace, one line each. |
| `/access` | — | The access mode and sandbox, fixed per process (ADR-0038). |
| `/modules` | `reload` | `/modules reload`: the line mode's module reload (ADR-0084). |
| `/NAME` for each skill | `what to do (optional)` | Present when the environment assembles the `skill` tool: one command per skill it lists. A turn for the model that asks it to load the skill with that tool and follow it, then the argument. |

`/model` and `/effort` appear only when the session has those config options. Every command but a skill reports as one `agent_message_chunk` and ends the prompt `end_turn` without a model turn; a failure is reported the same way, as text. A cancelled command answers `cancelled`. A skill command's turn is an ordinary prompt turn.

Every `session/update` and `session/request_permission` carries the `sessionId` of the session it belongs to. A request naming an unknown `sessionId` gets invalid params (`-32602`).

Questions (`ask_user_question`) take p1's headless path until #674. Workers have no ACP form until #681; their activity goes to stderr.

### Session usage

The stable ACP v1 [`usage_update`](https://agentclientprotocol.com/protocol/v1/prompt-turn#session-usage-updates) needs no extension declaration. [`fixtures/usage.jsonl`](fixtures/usage.jsonl) records tool-use and text responses, followed by unknown usage and recovery.

- `used` is the latest response's uncached input plus reported cache-read and cache-write tokens, not a lifetime sum. Output and reasoning tokens are not added. Cache categories a route does not report add nothing; unknown uncached input means unknown context usage.
- `size` is the parent's effective context window, including the selected profile's capacity, not its earlier summarization threshold. Unknown input usage or an unknown window sends no update.
- `cost`, when known, is cumulative parent-response spend for that session: `{"amount":0.00325,"currency":"USD"}`. p1 converts micro-USD to USD. A response with unknown usage or cost makes cumulative cost unknown for the rest of that session; subsequent updates omit `cost`, never substitute zero or publish a partial total. A reported known zero remains zero.
- Session state is independent across prompts and sessions. Workers' context and costs are excluded until #681. Usage updates are flushed before the prompt's reply, like other updates.

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
