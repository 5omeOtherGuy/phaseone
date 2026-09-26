---
adr: 81
title: Native foundation and runtime components
status: accepted
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [DECISIONS.md D22, docs/adr/0002-one-small-core-tools-and-providers-as-modules.md, docs/adr/0004-compile-time-composition-one-root.md, docs/adr/0035-the-shell-tool-can-run-inside-a-bubblewrap-execution-boundary.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0077-builds-on-the-stream-boxes.md, docs/design/modules/README.md, docs/design/modules/package.md, docs/design/modules/protocol.md, docs/design/modules/wit.md, docs/design/modules/toolchain.md, scripts/check-core-isolation.sh, modules/Cargo.toml, modules/toolchain.pins, crates/p1-module-runtime/src/lib.rs]
---
# ADR-0081: Native foundation and runtime components

## Context

ADR-0002 decided that `p1-core` runs one loop and depends only on `p1-contracts`, that every
tool is its own crate, and that providers translate wire behaviour only; ADR-0004 made the
composition compile-time and `p1-host` the single root. ADR-0071 then superseded ADR-0004's
compile-time-only rule: every tool, provider, context policy and authorization policy becomes a
WebAssembly module the host loads by name. ADR-0071 states the reading in one line — "ADR-0002
stands: the core still depends only on contracts; the runtime lives in the host" — but leaves
two things open that S1–S7 need frozen before they write a guest: which components the migration
does not move, and what a guest is allowed to be.

Some parts of p1 cannot become modules at all. A module has no process spawn and no sockets,
and a module must never hold a credential; the bubblewrap boundary, the workspace and the
journal are native services whose whole point is that the guest cannot reach past them. Read
carelessly, D22's decision that p1 migrates completely to WebAssembly modules would either pull a
capability into the guest — where it could be bypassed — or pull the wasmtime runtime into
`p1-core`, which ADR-0002 forbids and `scripts/check-core-isolation.sh` fails.

The guest side needs its own line. The owner's programme answered two questions that fix it
(XO with owner authority, 2026-09-25): the guest target is `wasm32-unknown-unknown` componentized
with `wasm-tools component new`, not `wasm32-wasip2` (D-XO-4 on S0-Q9), and a guest crate may use
only serde, serde_json and regex, at the versions the root lock pins (D-XO-8). S3 then asked
(S0-R3, published in [`package.md`](../design/modules/package.md)) whether guest logic may also
be a target-independent crate the native tests and the component share.

## Decision

The migration moves **extension implementations** and nothing else. Every tool, provider,
context policy and authorization policy becomes a WebAssembly module; **enforcement, transport,
interpreters and OS services stay native**. Concretely, these stay native:

| Native component | Why it cannot be a guest |
|---|---|
| The bubblewrap execution boundary (ADR-0035, `p1-tool-shell`) | It is the sandbox a module's command runs inside; a module cannot choose or weaken it. The host chooses bubblewrap, rebuilds the environment from the allow-list, sets the working directory and kills the process group. |
| The transport broker (`p1-provider-http` with `p1-auth`) | Sending, retry, backoff, the one credential refresh after a 401 or 403, the read bounds and the connection's lifetime are native; a module only lowers a request and classifies events (freeze item 9, [`wit.md`](../design/modules/wit.md)). A module never receives a credential. |
| The workflow interpreter (`p1-workflow`) | It keeps all run state and drives a workflow; a workflow *implementation* or *decision* component is an optional module, but the interpreter and its state are not. |
| The process service (extracted from `p1-tool-shell`) | `spawn` of a `bash -lc` command and its streaming resource are host functions over the native process and bubblewrap code. |
| Workspace confinement and atomic writes (`p1-workspace`) | Paths are resolved after symlinks and refused outside the workspace, and every write is the native atomic replacement; the guest sees only the capability. |
| Credentials (`p1-auth`) | The credential source is the host's; no interface returns a value. |
| The journal (`p1-journal`) | The session record is native and is the single truth (ADR-0021), including the version record and the assembly identity of ADR-0080 ([`journal.md`](../design/modules/journal.md)). |

`p1-core` still depends only on `p1-contracts`, and the WebAssembly runtime lives in the host:
wasmtime, the loader, the executor and the per-contract adapters are in `p1-module-runtime`,
never in the core. The core's view of a module is an ordinary `p1_contracts` trait object
constructed at the host's one composition root, exactly as ADR-0002 and ADR-0004 require; what
changed is one layer below the composition root, where the module is no longer linked in.

A guest has no WASI surface and no std I/O by design. The guest target is
`wasm32-unknown-unknown` (`WASM_TARGET` in `modules/toolchain.pins`, pinned by PR #252, merge
commit `4872fd78`), componentized by the build with `wasm-tools component new`; a component with
any `wasi:` import is refused by the build, the loader and the boundary check, not allow-listed
(D-XO-4; [`toolchain.md`](../design/modules/toolchain.md),
[`capabilities.md`](../design/modules/capabilities.md)). A guest panic is the wasm
`unreachable` trap, which the host maps through `ModuleFailure` into the existing closed shapes —
`ToolOutcome` for a tool call and `ProviderErrorKind` for a provider outcome — and adds no kind
([`protocol.md`](../design/modules/protocol.md)).

A guest crate may use only serde, serde_json and regex (D-XO-8); any other crate is a
programme question, not a judgment call. Guest logic that belongs to more than one build target
may live in a target-independent library crate under `crates/`, shared by the native adapter's
tests and the component package (S0-R3, [`package.md`](../design/modules/package.md)).

This ADR supplements ADR-0071 and does not supersede ADR-0002. ADR-0002's decision — a small
core that depends only on contracts, tools and providers as units of composition — is unchanged
by the migration; this records the compile-time consequence of reading it beside ADR-0071.

## Consequences

- The core stays provider-, tool- and runtime-neutral, and `scripts/check-core-isolation.sh`
  keeps a leaking wasmtime or module dependency a red gate rather than a convention.
- A native component keeps the OS surface a module cannot have, so confinement, credentials,
  the journal and the sandbox remain enforceable where the guest cannot reach them; the cost is
  that those components are not replaceable by a module without a new decision.
- A module can be added, removed or replaced without rebuilding the host, which is the gain
  ADR-0071 wanted; a *native* component still needs a rebuild.
- The toolchain must carry `wasm32-unknown-unknown`; a guest that links std keeps only what
  compiles without an OS, so anything needing I/O goes through an imported capability instead.
- A guest panic needs no unwinding and no error channel: it arrives as a trap and is classified
  by the host into kinds the core, the journal and the UI already branch on.
- Bounding a guest crate to three dependencies keeps the guest's dependency surface small and
  auditable, at the cost of rewriting a helper that would otherwise pull in a crate.
- A shared guest-logic crate lets the frozen native tests keep exercising the code the component
  ships; it must stay pure computation, or it breaks the guest build.

## Alternatives considered

- Put the runtime in `p1-core`, so a module is constructed like any other module at the root:
  rejected; it would make the core depend on wasmtime and fail ADR-0002 and the isolation check.
- Build guests for `wasm32-wasip2` and stow WASI behind trap stubs: rejected by D-XO-4; a
  permitted WASI surface would let a guest import a capability the capability model does not
  grant, and a std residue would drift.
- Let a module own its confinement, its writes or its transport: rejected; enforcement and
  transport stay native so a module cannot weaken either.
- Give the boundary a new error kind of its own: rejected; `ToolStatus` and `ProviderErrorKind`
  are closed sets the rest of p1 branches on, so a module failure maps into them
  ([`protocol.md`](../design/modules/protocol.md)).
- Let a guest use any crate (as a native crate may): rejected by D-XO-8's three-crate bound.

## Evidence

The published freeze is indexed in [`docs/design/modules/README.md`](../design/modules/README.md).
Every S0 PR, with its merge commit on main and the main `gate` run of that commit:

- S0.1, PR #205, merge commit `0c359b8d` (`0c359b8df904acdc9badb1f21b9fcca9b8758ed5`): ADR-0077,
  `scripts/module-toolchain.sh` and the gate's module hook; main gate run 36160382357, success.
- S0.3.1, PR #209, merge commit `07881e99` (`07881e99f5c26e5fe8e27ed2d2e608ee217e7fa7`): the
  `p1-module-protocol` crate, the closed error mapping and the JSON schema bundle of
  [`protocol.md`](../design/modules/protocol.md); main gate run 36166930390, success.
- S0.2, PR #218, merge commit `89922da2` (`89922da286baac96758e9722872776fdc3b6c509`):
  `p1-module-runtime` on wasmtime 49.0.1 with the component-model and async features, no
  `cache` and no WASI; main gate run 36173616397, success.
- S0.3.2, PR #226, merge commit `2f9c2223` (`2f9c2223b42c0ff703417c32b9ea108ff38f0f59`): the WIT
  package, whose worlds give a guest no `wasi:` interface ([`wit.md`](../design/modules/wit.md));
  its own main gate run 36176731212 was cancelled by the concurrency group and is covered by the
  descendant `e850cb88` run 36176937286, success.
- S0.4, PR #240, merge commit `a64d9e76` (`a64d9e7607ebc3437f8f9c003972490fc665e239`): the module
  workspace with `unsafe_code = "forbid"` and `panic = "abort"`, the fixture package and
  [`package.md`](../design/modules/package.md); main gate run 36188665897, success.
- S0.3.3, PR #245, merge commit `36ec6e16` (`36ec6e1626154989d54222a94c1e33353e41a88a`): the WIT
  amendments S0-R1 and S0-R2, including the component/broker split of the WebSocket decisions;
  main gate run 36190044937, success.
- S0.5, PR #252, merge commit `4872fd78` (`4872fd780581c39cc52df068b197ab2573a28e62`): the loader,
  executor, restricted path and `WasmTool` in `p1-module-runtime`, which put the runtime in the
  host, and the `wasm32-unknown-unknown` pin of D-XO-4; main gate run 36199422718, success.
- S0.7, PR #270, merge commit `e8682c7e` (`e8682c7e8d2f98e64fb4159cfdef53c61752fe38`):
  `scripts/check-module-boundaries.sh`, `modules/capabilities.toml` and
  [`capabilities.md`](../design/modules/capabilities.md), with S0-R4 and S0-R5; main gate run
  36207266519, success.
- S0.6, PR #272, merge commit `a808a2ed` (`a808a2ed369fb4d94133632feb3d5fb0eef45b80`):
  cancellation and the streaming-resource contract
  ([`cancellation.md`](../design/modules/cancellation.md)); its own main gate run 36208468329 was
  cancelled by the concurrency group and is covered by the descendant `acbe4304` run
  36208581179 (S0.8's), success.
- S0.8, PR #271, merge commit `acbe4304` (`acbe43049258c3fbc1e045bdbc393b5d855b388b`):
  `scripts/bench-modules.sh` and [`baseline.md`](../design/modules/baseline.md); main gate run
  36208581179, success.
- S0.9, PR #269 (issue #250): this ADR, the index and the remaining freeze documents; S0-R3 is
  published in [`package.md`](../design/modules/package.md), and the first shared guest crate
  lands with S3's slices.
- Acceptance: this ADR is accepted with slice S0.9 on the `judge` role's ACCEPT of that slice;
  the tag `wasm-boundary-v1` is cut on S0.9's merge commit only after it.
- The recorded definition-of-done runs, gate logs and verdicts are the evidence bundle
  `.wasm/up/evidence/` on box wasm-s0.
