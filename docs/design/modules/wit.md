# Module WIT: worlds, capabilities and streaming resources

Status: published freeze items 1, 3 and 10 of the WebAssembly boundary (ADR-0071), with the
per-class capability allocation of item 13 and the restricted path of item 4. The package is
[`modules/wit/`](../../../modules/wit/); the values it carries are described in
[`protocol.md`](protocol.md).

## The package

One WIT package, `p1:module@1.0.0`. Its major version is the major of `PROTOCOL_VERSION` in
`p1-module-protocol` (currently 1), so a module built against this package speaks protocol
major 1. The files are split by topic:

| File | Contents |
|---|---|
| [`types.wit`](../../../modules/wit/types.wit) | `types`: the JSON aliases and the small closed values every world shares |
| [`runtime.wit`](../../../modules/wit/runtime.wit) | `control`, `clock`, `random`, `notices` |
| [`workspace.wit`](../../../modules/wit/workspace.wit) | `workspace`, `snapshot`, `workspace-mutation` |
| [`process.wit`](../../../modules/wit/process.wit) | `process` |
| [`transport.wit`](../../../modules/wit/transport.wit) | `credential-control`, `http`, `websocket` |
| [`session.wit`](../../../modules/wit/session.wit) | `summary`, `completion` |
| [`delegation.wit`](../../../modules/wit/delegation.wit) | `workers`, `workflows` |
| [`decoding.wit`](../../../modules/wit/decoding.wit) | `decoding`, the provider's exported decoder |
| [`worlds.wit`](../../../modules/wit/worlds.wit) | the five worlds |

No world imports a `wasi:` interface. WASI is not part of the approved dependency set and the
host does not link `wasmtime-wasi`, so every capability a module can have is one of p1's own
interfaces below. Whether a guest built for `wasm32-wasip2` avoids every `wasi:` import is
checked on the built component (the fixture module and `scripts/check-module-boundaries.sh`),
not here; the worlds give such a guest nothing to import from WASI.

## Two rules for every world

**Synchronous exports, asynchronous host imports.** Every export is a plain WIT function the
host calls on the module's one executor. A host import may block the module while the native
host awaits: wasmtime's asynchronous host functions suspend the guest, so waiting for a
process, a worker, a summary or the write gate is an ordinary import call. No world uses WIT
`future` or `stream` types.

**Rich values cross as JSON text.** Each value family of [`protocol.md`](protocol.md) has a
string alias in `types` (`tool-call`, `tool-outcome`, `history-item`, `stream-event`,
`provider-error`, `call-description`, `result-description`, `model-options`,
`route-description`, `usage`), and the text must conform to that family's schema
(`p1:protocol/<family>/1`). Replay data travels inside history items and stream events, so it
has no alias of its own. Text that does not conform is the module's invalid output
(`ModuleFailure::InvalidOutput`), never a value. Small closed values that the host reads
without parsing JSON are WIT types: `effect` (`read-only`, `writes-files`, `executes`,
`delegates`, mirroring `p1_contracts::Effect`), `decision` (`permit` or `deny(reason)`), the
declaration and its `declaration-kind`, `tool-identity` and `stop-reason`.

The generic alias `json` marks JSON that is not a protocol family; each use says what it
holds: a declaration's input JSON Schema, a module's own settings, a workflow's arguments and
value, a finish tool's output contract and structured result, and a workflow run's status
(`p1_workflow::RunStatus` in its serde form, whose schema `p1-workflow` owns).

## Worlds (freeze item 1)

| World | Exports | Native trait the host adapter implements |
|---|---|---|
| `tool` | `declaration`, `effect`, `describe`, `describe-result`, `execute` | `p1_contracts::Tool` |
| `provider` | `configure`, `describe`, `validate`, `lower`, `classify`, interface `decoding` | `p1_contracts::Provider` |
| `context-policy` | `configure`, `prepare`, `compact-now` | `p1_contracts::ContextPolicy` |
| `authorization-policy` | `authorize` | `p1_contracts::AuthorizationPolicy` |
| `workflow-implementation` (optional) | `run` | none yet |

- **tool.** `declaration` returns what the model is told; `effect` and `describe` classify
  and describe one call; `describe-result` describes a `tool_result` history item; `execute`
  takes a tool call and returns a tool outcome. The tool's identity is built by the loader
  from the package identity and is not exported, so a module cannot claim another
  implementation's identity or grants.
- **provider.** A provider module lowers requests and classifies what comes back; the broker
  sends (freeze item 9, below). `describe` returns the route description; `validate` refuses
  what the route cannot carry; `lower` turns a provider request into the HTTP request the
  broker sends (method, path relative to the route's endpoint, headers without credentials,
  the credential's placement, body) and, for a route that speaks WebSocket, the handshake and
  request frame as well; `classify` maps a non-2xx response or a refused upgrade to a provider
  error; the exported `decoding` interface holds the `decoder` resource that turns the
  response's events into stream events (`feed`, and `finish` when the body ends without a
  terminal event).
- **context-policy.** `prepare` takes the history and the last response's usage and returns
  `none` (unchanged), replacement items with the usage preparing them cost, or a context error
  (`cancelled` or `failed`). `compact-now` is the manual compaction of ADR-0076.
- **authorization-policy.** `authorize` takes the call, the loader-built identity of the tool
  that would run it and that tool's effect, and returns a decision. Asking the operator stays
  in the host's own policy; this world has no way to read input.
- **workflow-implementation.** Optional: a workflow written as a module instead of a script.
  `run` takes the arguments and returns the workflow's JSON value or why it failed; it works
  through `workers` and `workflows`. No stream needs it yet, and the host needs no adapter for
  it until one does.

Additions beyond the slice brief's export list, each with its reason:

- `configure` (provider, context-policy). A native provider is constructed for one route and
  model, and a native context policy with its thresholds; none of the listed exports receives
  them, and a route description cannot be produced without them. The host calls `configure`
  once, before any other export, and an `err` refuses the assembly. Tools and authorization
  policies need none: their native constructors take only host services, which are imports
  here, and an environment's tool face is applied by the host.
- `classify` (provider). The native `ResponseParser::on_http_error` classifies a non-2xx
  response by route knowledge (a context-window error type in the body); the broker cannot.
- `compact-now` (context-policy). `ContextPolicy` has it and the native summarizing policy
  implements it; without it a module policy would silently lose `/compact`.
- The decoder is fed framed events, not raw bytes: SSE framing is transport code every route
  shares (`p1-provider-http`'s SSE decoder), so the broker frames and the decoder receives an
  SSE event with its name, or one WebSocket text frame as data with no name, exactly as the
  native `ResponseParser` does.
- `websocket-request.continuation` and `decoder.response-id`. The native Codex route continues
  the previous response on a reused connection (`docs/design/websocket.md` §6). The connection
  is the broker's, so the module offers a continuation frame naming the response it continues,
  the decoder reports the id of the response it saw, and the broker sends the continuation only
  on the connection whose last clean response has that id; otherwise the full frame goes out.

## Capability interfaces (freeze item 3)

Every capability is an interface of this package; its doc comment in the WIT names the owning
native crate. Each is sized to what today's native implementation needs.

| Interface | Owner | What it offers |
|---|---|---|
| `control` | `p1-module-runtime` | `cancelled()`: the cooperative cancellation check |
| `clock` | `p1-module-runtime` | the wall clock (`now`) and a monotonic clock (`monotonic-now`) |
| `random` | `p1-module-runtime` | `bytes(len)` for nonces and ids |
| `notices` | host event layer | `notice(text)`: a display-only operator notice (ADR-0048) |
| `workspace` | `p1-workspace` | `stat`, windowed `read`, `list-files` (the gitignore-aware walk) and `search`, all confined |
| `snapshot` | `p1-workspace` | the observed-file registry: `observe`, `check` |
| `workspace-mutation` | `p1-workspace` | `begin` the write gate; the `mutation` resource's `write`, `create`, `remove`, `rename`, each an atomic native operation |
| `process` | the process service extracted from `p1-tool-shell` | `spawn` a `bash -lc` command with a time limit; the `running` streaming resource |
| `http` | `p1-provider-http` with the `p1-auth` broker | the lowered HTTP request and the response head |
| `websocket` | `p1-provider-http` with the `p1-auth` broker | the lowered WebSocket handshake, frame and continuation |
| `credential-control` | `p1-provider-http` with the `p1-auth` broker | `credential-use`: how the broker attaches the credential |
| `summary` | the native context adapter | `summarize`: one summarization request through the agent's provider |
| `completion` | the host completion hub | the session record the `finish` tool verifies against, and `accept` |
| `workers` | `p1-workers` | `start`, `describe`, `status`, `wait`, `cancel`, `continue-child` |
| `workflows` | `p1-workflow` | `start`, `status`, `wait`, `cancel` |

Notes on the boundary each one keeps:

- **Confinement is the host's.** Every `workspace`, `snapshot` and `workspace-mutation` path
  is resolved after symlinks by `p1-workspace`, so an escaping path is `outside-workspace`
  whatever the module does. Writes are the native atomic replacement; the write gate is held
  through a `mutation` resource from reading a file to recording the write, as the native
  file tools do.
- **The execution boundary stays native** (ADR-0035). A module chooses a command's text and
  time limit; the host chooses bubblewrap or not, rebuilds the environment from the
  allow-list, sets the working directory and kills the process group.
- **A module never receives a credential.** `http`, `websocket` and `credential-control`
  carry no function that returns a value: a provider names where the credential goes
  (`credential-use`), and the broker refuses a lowered request whose own headers name
  `authorization`, `proxy-authorization`, `cookie`, `x-api-key` or `api-key`. Sending,
  retry, backoff, the one refresh after a 401 or 403, the read bounds, the WebSocket
  connection's lifetime and the fallback to HTTP all stay in the broker (freeze item 9).
- **Summaries are masked natively.** The host masks credential-shaped text in a summary before
  the module sees it, as the native policy does (issue #142).
- **Worker and workflow waits block the import.** `wait` returns as soon as the child or run
  is no longer running, and returns the running status at once when the call is cancelled
  first, as `WorkerService::wait` and `WorkflowService::wait` do.

`types` is imported by every world for its types only and grants nothing, so it is not part of
any allocation.

## Per-class capability allocation (freeze item 13)

Each world imports the union of the capabilities its class may be granted. A module's manifest
narrows it, the host links only what the manifest grants, and
`scripts/check-module-boundaries.sh` compares a component's actual imports to this table.

| Capability | tool | provider | context-policy | authorization-policy | workflow-implementation |
|---|---|---|---|---|---|
| `control` | yes | yes | yes | yes | yes |
| `clock` | yes | yes | yes | yes | yes |
| `random` | yes | yes | — | — | yes |
| `notices` | yes | yes | yes | yes | yes |
| `workspace` | yes | — | — | — | — |
| `snapshot` | yes | — | — | — | — |
| `workspace-mutation` | yes | — | — | — | — |
| `process` | yes | — | — | — | — |
| `workers` | yes | — | — | — | yes |
| `workflows` | yes | — | — | — | yes |
| `completion` | yes | — | yes | — | — |
| `http` | — | yes | — | — | — |
| `websocket` | — | yes | — | — | — |
| `credential-control` | — | yes | — | — | — |
| `summary` | — | — | yes | — | — |

The allocation is the frozen one, unchanged.

## Cancellation and the restricted path (freeze item 4, the WIT part)

Epoch deadlines and fuel are enforced by the host and need no WIT. The cooperative part is
`control.cancelled()`: once true it stays true for the rest of the call, and a module doing
long computation between imports checks it and returns promptly. Blocking imports return on
their own when the call is cancelled, as each documents, so a module never polls while it
waits. A trap never undoes a native effect: what a command, a write or a worker already did
stays done.

The host calls a tool's `effect` and `describe`, and a provider's `describe`, on a restricted
path: no capability is granted and the fuel budget is tight, so any import called there traps
and a module's inspection code cannot reach a capability. These exports work from their
arguments alone. What only a capability can know, such as whether a path escapes the
workspace through a symlink, is judged lexically there and enforced again by the capability
when the call executes.

## Streaming resources (freeze item 10)

A long-running output is a WIT resource with a blocking `next: func() -> option<…>`. The
instance in this package is `process.running`; HTTP bodies and WebSocket connections are not
resources a module holds, because the broker owns sending (freeze item 9). The provider's
`decoder` is the same idea in the other direction: the host feeds it, and it answers.

- **Create.** One import call creates the resource (`process.spawn`).
- **Blocking `next`.** `next` blocks the module while the host waits. It returns items
  (`output`), then exactly one terminal item (`exited`), then `none`.
- **Cancel.** When the call is cancelled while `next` is blocked, the host ends the work (kills
  the process group) and `next` returns what remains, then the terminal item (`exited` with
  `cancelled`).
- **Drop, and drop while blocked.** Dropping the resource ends the work if it still runs. When
  the host abandons a call while `next` is blocked (an epoch deadline, or the caller dropping
  the call), it drops the resource itself with the same effect. A resource does not outlive
  the export call that created it: when that call returns, the host drops what the module
  still holds, and a later use traps. The same call-scoped rule holds for a
  `workspace-mutation.mutation`, which releases the write gate.
- **Trap after terminal.** Calling `next` again after it returned `none` traps. The broker
  never calls a decoder's `feed` or `finish` after a terminal event, and a decoder may trap if
  it is.

## Checks

- `wasm-tools component wit modules/wit` resolves the package.
- For each world, `wasm-tools component embed --dummy --world <world> modules/wit`, then
  `wasm-tools component new` and `wasm-tools validate`, form a valid component whose
  extracted world imports no `wasi:` interface.
- `scripts/module-toolchain.sh --check` records the digest of every `.wit` file.
