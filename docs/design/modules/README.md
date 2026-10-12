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
| 6 | Module package format and identity: manifest fields, digest as identity, loader-built `ToolIdentity`, reserved `p1/` namespace, verify-then-compile-same-bytes, only the release's digest-pinned compiled copy deserialized (ADR-0113), installed sets only, any publisher's package pinned by digest (F7; amended by ADR-0209) | [`package.md`](package.md), [The loader](package.md#the-loader-freeze-item-6), [Installed sets](package.md#installed-sets-adr-0209), [ADR-0082](../../adr/0082-component-abi-and-execution-ownership.md), [ADR-0209](../../adr/0209-external-modules-any-publisher-s-package-loads-by-its-lock-entry-verified-by-digest.md) |
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
| ADR-0209 | A package of any publisher loads from an installed set, pinned by digest in the user's lock; `p1/` stays the release's | [`package.md` — Installed sets](package.md#installed-sets-adr-0209), [Package archives](package.md#package-archives-and-p1-modules-install-adr-0209) |
| ADR-0210 | One guest SDK crate pinned by tag; a host serves its WIT and protocol major at every minor up to its own | [Writing a module outside this repository](#writing-a-module-outside-this-repository), [`protocol.md` — Version rule](protocol.md#version-rule), [`template/`](template/) |

## S0's ADRs

| ADR | Decides |
|---|---|
| [ADR-0077: Builds on the stream boxes](../../adr/0077-builds-on-the-stream-boxes.md) | where the gate, builds and tests run, and which gate variant runs where |
| [ADR-0081: Native foundation and runtime components](../../adr/0081-native-foundation-and-runtime-components.md) | extension implementations become modules; enforcement, transport, interpreters and OS services stay native; what a guest may be |
| [ADR-0082: Component ABI and execution ownership](../../adr/0082-component-abi-and-execution-ownership.md) | ADR-0015 at the boundary: one Store-owning executor, per-call limits, the restricted path, the loader's trust rules |

## Writing a module outside this repository

Anyone can build a p1 module in a repository of their own (owner 2026-10-11, epic #886). p1
loads it once a user installs it ([ADR-0209](../../adr/0209-external-modules-any-publisher-s-package-loads-by-its-lock-entry-verified-by-digest.md)),
and it is built against the published guest SDK
([ADR-0210](../../adr/0210-modules-build-on-one-sdk-crate-pinned-by-tag-a-host-serves-older-minors-of-its-major.md)).

1. **Start from the template.** Copy [`template/`](template/) into a new repository. Rename the
   crate (`[package] name`) and the package (`[package.metadata.p1-module] name =
   "<you>/<module>"`). The namespace is yours; `p1/` is reserved for p1's release.
2. **Pick the class and the world.** Set `kind`, the matching `world = "p1:module/<kind>@1.0.0"`
   and the SDK feature of the same class (`tool`, `provider`, `context-policy`,
   `authorization-policy`, `workflow-decision`). `capabilities` lists what the module may import,
   within its class's allocation ([`capabilities.md`](capabilities.md),
   [`modules/capabilities.toml`](../../../modules/capabilities.toml)). Anything beyond it is
   refused when the package loads.
3. **Implement the world.** Implement the world's `Guest` trait and export it with
   `p1_module_sdk::<world>::export!(T with_types_in p1_module_sdk::<world>)`. Every value is JSON
   in its protocol form ([`protocol.md`](protocol.md)); the SDK's `protocol` feature has the
   `Wire*` types and `serde_json`. A guest has no WASI: everything it reaches is an imported
   capability ([`wit.md`](wit.md)).
4. **Build.** `./build.sh` builds the component with the pinned toolchain
   (`scripts/build-modules.sh --external` of the SDK's checkout) and packs
   `target/p1-module/<crate>.p1pkg`.
5. **Check.** `./conformance.sh` runs p1's author conformance checks on the archive
   (`crates/p1-module-tests/tests/author_conformance.rs`). A tool's restricted-path answers are
   checked. A provider runs the provider conformance suite over its route, profile and SSE
   fixtures (`P1_CONFORMANCE_ROUTE`, `P1_CONFORMANCE_PROFILE`, `P1_CONFORMANCE_FIXTURES`).
6. **Publish and install.** Attach the archive to a release; the template's CI does this for a
   `v*` tag, with its SHA-256. A user runs `p1 modules install <file>`, which pins its digest in
   their `modules.lock` under the module key its name declares, and names that key in an
   environment.

**Versions.** The SDK is a tagged git dependency of this repository (`module-sdk-v<version>`; its
version is the WIT package's, now 1.0.0). A host serves a module whose WIT and protocol majors
are its own and whose minors are no newer than its own. Pin the oldest SDK tag whose features
the module needs: the module then runs on every p1 of that major from that tag on.
