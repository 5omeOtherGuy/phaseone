# The WebAssembly module boundary

Status: frozen at `wasm-boundary-v1`.

This directory publishes the boundary between p1's native host and its WebAssembly modules
(ADR-0071): what streams S1–S7 build against. The tag `wasm-boundary-v1` is cut on the merge
commit of slice S0.9 after the `judge` role accepts it; from then on a change to anything
indexed here is a boundary change and takes the boundary's own review, not an ordinary edit.

## Documents

| Document | What it publishes |
|---|---|
| [`protocol.md`](protocol.md) | the value families and their version rule, the error mapping, the call verb vocabulary |
| [`wit.md`](wit.md) | the WIT package: worlds, capability interfaces, the allocation, the transport split, streaming resources |
| [`cancellation.md`](cancellation.md) | how a call is bounded and cancelled, and the restricted path |
| [`package.md`](package.md) | the module package, its manifest and identity, the loader, the guest target, shared guest logic |
| [`journal.md`](journal.md) | the journal's version record and assembly identity, and the gaps S1 closes |
| [`toolchain.md`](toolchain.md) | the toolchain pins and the lint policy for generated bindings |
| [`capabilities.md`](capabilities.md) | the allocation as frozen data, the boundary check and the unsafe policy |
| [`adapters.md`](adapters.md) | the generic tool adapter `WasmTool` and where the other adapters go |
| [`baseline.md`](baseline.md) | the cold and warm cost of the runtime, recorded as facts (a DoD record, not a freeze item) |

## The freeze list

One row per item of the S0 brief's freeze list (§6).

| # | Freeze item | Published in |
|---|---|---|
| 1 | WIT worlds per module kind; synchronous exports, asynchronous host imports | [`wit.md` — Worlds](wit.md#worlds-freeze-item-1), [Two rules for every world](wit.md#two-rules-for-every-world) |
| 2 | The JSON schema bundle for the rich contract values, with versions | [`protocol.md` — Value families](protocol.md#value-families-freeze-item-2), [Version rule](protocol.md#version-rule) |
| 3 | The capability interfaces with an owning native crate per interface (F9) | [`wit.md` — Capability interfaces](wit.md#capability-interfaces-freeze-item-3) |
| 4 | Cancellation semantics: epoch deadline, fuel, cooperative `control.cancelled()`, a trap never undoes a native effect; the restricted path for `effect` and `describe` (F10) | [`cancellation.md`](cancellation.md), [`wit.md` — the WIT part](wit.md#cancellation-and-the-restricted-path-freeze-item-4-the-wit-part), [ADR-0082](../../adr/0082-component-abi-and-execution-ownership.md) |
| 5 | Error mapping into the closed `ProviderErrorKind` and `ToolOutcome` shapes; no new error kind | [`protocol.md` — Error mapping](protocol.md#error-mapping-freeze-item-5) |
| 6 | Module package format and identity: manifest fields, digest as identity, loader-built `ToolIdentity`, reserved `p1/` namespace, verify-then-compile-same-bytes, no compiled-cache deserialization, official source only (F7) | [`package.md`](package.md), [The loader](package.md#the-loader-freeze-item-6), [ADR-0082](../../adr/0082-component-abi-and-execution-ownership.md) |
| 7 | Journal: a version-bearing record old binaries fail on, and the assembly identity record (F3) | [`journal.md`](journal.md), [ADR-0080](../../adr/0080-execution-manifests-in-journals.md) |
| 8 | The verb vocabulary for call descriptions: a closed vocabulary, no public type change (F4) | [`protocol.md` — Call verb vocabulary](protocol.md#call-verb-vocabulary-freeze-item-8) |
| 9 | Retry, backoff and one refresh on 401 stay native in the transport broker; components lower requests and classify events only (F2) | [`wit.md` — Transport](wit.md#transport-retry-backoff-and-the-one-refresh-stay-native-freeze-item-9), [WebSocket: who decides what](wit.md#websocket-who-decides-what), [ADR-0081](../../adr/0081-native-foundation-and-runtime-components.md) |
| 10 | The streaming-resource fixture: create, `next()` blocked on a host wait, cancel; drop while blocked; trap after terminal (F6) | [`wit.md` — Streaming resources](wit.md#streaming-resources-freeze-item-10), [`cancellation.md` — Tests](cancellation.md#tests) |
| 11 | Toolchain pins; the lint policy for generated bindings under `unsafe_code = "forbid"` | [`toolchain.md`](toolchain.md), [`capabilities.md` — The unsafe policy](capabilities.md#the-unsafe-policy-freeze-item-11) |
| 12 | The generic tool adapter `WasmTool` in `p1-module-runtime`; the provider adapter is S4's, the policy adapters S5's | [`adapters.md`](adapters.md) |
| 13 | The capability allocation per module class as frozen data, compared by `scripts/check-module-boundaries.sh` | [`capabilities.md`](capabilities.md#the-allocation-as-frozen-data-freeze-item-13), [`wit.md` — Per-class allocation](wit.md#per-class-capability-allocation-freeze-item-13), [`modules/capabilities.toml`](../../../modules/capabilities.toml) |

## Decisions

Questions and requests answered before the freeze, and where each is published.

| Decision | What it decided | Published in |
|---|---|---|
| S0-Q9 / D-XO-4 | The guest target is `wasm32-unknown-unknown`, componentized with `wasm-tools component new` and no WASI adapter; every `wasi:` import is refused | [`package.md` — The guest target](package.md#the-guest-target-s0-q9), [`toolchain.md`](toolchain.md#the-guest-target-d-xo-4), [`capabilities.md`](capabilities.md#why-wasi-is-always-refused-d-xo-4), [ADR-0081](../../adr/0081-native-foundation-and-runtime-components.md) |
| S0-R1 | The `workflow-decision` world; host-scoped worker ids; the worker interface split | [`wit.md` — Decisions](wit.md#decisions-s0-r1-s0-r2-s0-r4-and-s0-r5) |
| S0-R2 | The authorization `verdict` with `ask`; continuation and HTTP fallback in the provider component | [`wit.md` — Decisions](wit.md#decisions-s0-r1-s0-r2-s0-r4-and-s0-r5), [WebSocket: who decides what](wit.md#websocket-who-decides-what) |
| S0-R3 | Guest logic may be a target-independent library crate under `crates/` | [`package.md` — Shared guest logic](package.md#shared-guest-logic-s0-r3), [ADR-0081](../../adr/0081-native-foundation-and-runtime-components.md) |
| S0-R4 | `workflow-decision` as a manifest kind; the worker split in the frozen data; its bindings crate | [`wit.md` — Decisions](wit.md#decisions-s0-r1-s0-r2-s0-r4-and-s0-r5), [`capabilities.md`](capabilities.md) |
| S0-R5 | The `summary` doc comment: the host sends the module's cap as given; the module owns the retry | [`wit.md` — Decisions](wit.md#decisions-s0-r1-s0-r2-s0-r4-and-s0-r5), [`modules/wit/session.wit`](../../../modules/wit/session.wit) |

## S0's ADRs

| ADR | Decides |
|---|---|
| [ADR-0077: Builds on the stream boxes](../../adr/0077-builds-on-the-stream-boxes.md) | where the gate, builds and tests run, and which gate variant runs where |
| [ADR-0081: Native foundation and runtime components](../../adr/0081-native-foundation-and-runtime-components.md) | extension implementations become modules; enforcement, transport, interpreters and OS services stay native; what a guest may be |
| [ADR-0082: Component ABI and execution ownership](../../adr/0082-component-abi-and-execution-ownership.md) | ADR-0015 at the boundary: one Store-owning executor, per-call limits, the restricted path, the loader's trust rules |
