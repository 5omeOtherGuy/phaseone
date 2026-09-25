# Module protocol: values, errors and verbs

Status: published freeze items 2, 5 and 8 of the WebAssembly boundary (ADR-0071).
Implemented by the `p1-module-protocol` crate; the WIT worlds that carry these values
follow separately.

Every rich contract value that crosses the boundary between the native host and a
WebAssembly module travels as JSON in the form this document describes. The crate's
`Wire*` types are the only serialized form of these values: `p1-contracts` stays the
native form, and several of its types (`StreamEvent`, `Outcome`, `ToolOutcome`) have no
serde form at all. Conversions both ways are lossless for every value `p1-contracts`
can hold, with the one deliberate exception of call verbs (below).

## Version rule

`PROTOCOL_VERSION` is (major, minor), currently major 1, minor 0. Every boundary value is
interpreted under it, and every schema `$id` carries the major:
`p1:protocol/<family>/<major>`.

- A **major** change is any change to a value an existing peer already accepts: a
  renamed or removed field, a changed type, a new required field, a changed meaning. A
  host refuses a module built for another major (the check belongs to the loader).
- A **minor** change adds something an older host refuses cleanly or shows neutrally,
  such as a new call verb.

Closed objects reject unknown fields, both in the schemas (`additionalProperties: false`)
and in serde (`deny_unknown_fields`). A module sending a field the host does not know is
speaking another version; dropping the field silently would hide that. Optional values
are omitted when absent, never sent as `null` or zero, so unknown usage stays unknown.

## Value families (freeze item 2)

One JSON Schema (draft 2020-12) per family. Families that embed another reference it by
its `$id`, into its `$defs` where only a part is shared. Tagged unions are `oneOf` with a
`const` tag.

| Family | Schema | Contract type | Tag |
|---|---|---|---|
| Tool call | [`tool-call.json`](../../../crates/p1-module-protocol/schema/tool-call.json) | `ToolCall` | input `kind`: `json`, `text`; `raw` kept verbatim |
| Tool outcome | [`tool-outcome.json`](../../../crates/p1-module-protocol/schema/tool-outcome.json) | `ToolOutcome` | `status`: the closed `ToolStatus` set |
| History item | [`history-item.json`](../../../crates/p1-module-protocol/schema/history-item.json) | `Item` | `item`: `user`, `inbox`, `assistant`, `tool_result`; assistant `block`: `text`, `reasoning`, `tool_call` |
| Stream event | [`stream-event.json`](../../../crates/p1-module-protocol/schema/stream-event.json) | `StreamEvent`, `Outcome` | `event`: `text_delta`, `reasoning_delta`, `tool_input_delta`, `notice`, `activity`, `finished`; outcome `status`: `completed` (item, stop, usage), `failed` (error), `cancelled` |
| Provider error | [`provider-error.json`](../../../crates/p1-module-protocol/schema/provider-error.json) | `ProviderError` | `kind`: the nine `ProviderErrorKind`s |
| Call description | [`call-description.json`](../../../crates/p1-module-protocol/schema/call-description.json) | `CallDescription` | `verb` from the closed vocabulary; `destructive` always stated |
| Result description | [`result-description.json`](../../../crates/p1-module-protocol/schema/result-description.json) | `ResultDescription` | detail `kind`: `diff`, `command`, `matches`, `files`, `text` |
| Model options | [`model-options.json`](../../../crates/p1-module-protocol/schema/model-options.json) | `ModelOptions` | absent field = the route's default |
| Route description | [`route-description.json`](../../../crates/p1-module-protocol/schema/route-description.json) | `RouteDescription` | — |
| Replay data | [`replay-data.json`](../../../crates/p1-module-protocol/schema/replay-data.json) | `ReplayData` | `origin {route, model}`, `version`, opaque `payload` |
| Usage | [`usage.json`](../../../crates/p1-module-protocol/schema/usage.json) | `Usage` | every field optional |

Notes on the shapes:

- **Replay payloads are opaque.** The schema places no constraint on `payload`; the host
  and the stores carry it verbatim. Only the adapter that wrote it interprets it, and
  `version` lets that adapter refuse a layout it no longer reads. It is valid only for
  its `origin`.
- **Indices and counts are unsigned 64-bit on the wire** (`block` of a delta, `count` of
  matches), independent of either peer's pointer width; converting into a narrower host
  `usize` fails rather than truncates.
- **`ResultDetail::Text`** is `{"kind": "text", "text": …}` on the wire, because a tagged
  object cannot hold a bare string.
- **Tool calls appear only in a completed item**, never in a delta: `tool_input_delta`
  is display only, as in the native stream rules.

The crate's tests hold one fixture per variant of every family, round-trip each through
the wire type and the contract type back to identical JSON, validate each against its
schema, and check that a rejected fixture per family (an unknown field, tag or kind) is
refused by both serde and the schema.

## Error mapping (freeze item 5)

What can fail on the module side is `ModuleFailure`. It maps totally into the existing
closed shapes; the boundary adds no `ProviderErrorKind` and no `ToolStatus`, because the
core, the journal, the UI and the native retry rules all branch on those sets and a new
kind would have no defined handling.

| Failure | Tool call result | Provider terminal outcome | Why |
|---|---|---|---|
| `Cancelled` | `ToolStatus::Cancelled`, empty content | `Outcome::Cancelled` | Nothing failed; the caller asked to stop. Empty content matches native tools. |
| `Trap(message)` | `ToolStatus::Error` naming the trap | `Protocol` | The guest is deterministic: the same request traps again, and `Protocol` is not retried. |
| `InvalidOutput(message)` | `ToolStatus::Error` naming the parse error | `Protocol` | Output that does not parse is a protocol violation; retrying the same request reproduces it. |
| `FuelExhausted` | `ToolStatus::Error` naming the compute budget | `Protocol` | Fuel counts instructions, not time, so the same request exhausts it again. |
| `DeadlineExceeded` | `ToolStatus::Error` naming the time limit | `Transport` | A wall-clock deadline depends on host waits such as the network, so the transport broker may retry, within its own policy. |
| `Host(error)` | `ToolStatus::Error` naming the kind and message | `error` unchanged | The host import already classified it; reclassifying would change its retry rule. |

Retry, backoff and the one refresh on 401 stay in the native transport broker; a module
only classifies. Tool error texts tell the model which failure happened and that effects
before it may be partial, because a trap never undoes a native effect. Every message is
produced by the host (the trap text, the parse error), never copied from guest memory or
from the invalid output itself.

## Call verb vocabulary (freeze item 8)

`CallDescription.verb` is a `&'static str`, and the UI keys its vocabulary on it. Rather
than change that public type (which would need an ADR), the boundary maps a module's verb
string onto a closed vocabulary: exactly the verbs p1's tools use —

`read`, `edit`, `run`, `search`, `finish`, `worker`, `workflow`, `call`.

`call_verb` returns the matching static; any other string becomes `call`, and the
original is kept nowhere. A module therefore cannot mint UI vocabulary. This is the one
lossy conversion: a native description with a verb outside the vocabulary becomes `call`
after a round trip.

Adding a verb is a protocol minor-version change: the verb is added to `CALL_VERBS`, to
this list and to the UI's handling in one change; an older host shows it as `call`.
