<!-- ADR draft: numbered at landing with scripts/adr.py new "Native foundation and runtime components" --deciders owner+lead -->
sources: [DECISIONS.md D22, docs/adr/0002-one-small-core-tools-and-providers-as-modules.md, docs/adr/0004-compile-time-composition-one-root.md, docs/adr/0035-the-shell-tool-can-run-inside-a-bubblewrap-execution-boundary.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/design/modules/package.md, docs/design/modules/protocol.md, docs/design/modules/wit.md, scripts/check-core-isolation.sh, modules/Cargo.toml, modules/toolchain.pins, crates/p1-module-runtime/src/lib.rs]
# Native foundation and runtime components

## Context

ADR-0002 decided that `p1-core` runs one loop and depends only on `p1-contracts`, that every
tool is its own crate, and that providers translate wire behaviour only; ADR-0004 made the
composition compile-time and `p1-host` the single root. ADR-0071 then superseded ADR-0004's
compile-time-only rule: every tool, provider, context policy and authorization policy becomes a
WebAssembly module the host loads by name. ADR-0071 states the reading in one line — "ADR-0002
stands: the core still depends only on contracts; the runtime lives in the host" — but leaves
two things open that S1–S7 need frozen before they write a guest: which components the migration
does not move, and what a guest is allowed to be.

Some parts of p1 cannot become modules at all. WASI gives a module no process spawn and no
sockets, and a module must never hold a credential; the bubblewrap boundary, the workspace and
the journal are native services whose whole point is that the guest cannot reach past them. Read
carelessly, "everything migrates to wasm" (DECISIONS.md D22) would either pull a capability into
the guest — where it could be bypassed — or pull the wasmtime runtime into `p1-core`, which
ADR-0002 forbids and `scripts/check-core-isolation.sh` fails.

The guest side needs its own line. The owner's programme answered two questions that fix it
(XO with owner authority, 2026-09-25): the guest target is `wasm32-unknown-unknown` componentized
with `wasm-tools component new`, not `wasm32-wasip2` (D-XO-4), and a guest crate may use only
serde, serde_json and regex, at the versions the root lock pins (D-XO-8). S3 then asked (S0-R3,
recorded in [`package.md`](../package.md)) whether guest logic may also be a target-independent
crate the native tests and the component share.

## Decision

The migration moves **extension implementations** and nothing else. Every tool, provider,
context policy and authorization policy becomes a WebAssembly module; **enforcement, transport,
interpreters and OS services stay native**. Concretely, these stay native:

| Native component | Why it cannot be a guest |
|---|---|
| The bubblewrap execution boundary (ADR-0035, `p1-tool-shell`) | It is the sandbox a module's command runs inside; a module cannot choose or weaken it. The host chooses bubblewrap, rebuilds the environment from the allow-list, sets the working directory and kills the process group. |
| The transport broker (`p1-provider-http` with `p1-auth`) | Sending, retry, backoff, the one credential refresh after a 401 or 403, the read bounds and the connection's lifetime are native; a module only lowers a request and classifies events (freeze item 9). A module never receives a credential. |
| The workflow interpreter (`p1-workflow`) | It keeps all run state and drives a workflow; a workflow *implementation* is an optional module, but the interpreter and its state are not. |
| The process service (extracted from `p1-tool-shell`) | `spawn` of a `bash -lc` command and its streaming resource are host functions over the native process and bubblewrap code. |
| Workspace confinement and atomic writes (`p1-workspace`) | Paths are resolved after symlinks and refused outside the workspace, and every write is the native atomic replacement; the guest sees only the capability. |
| Credentials (`p1-auth`) | The credential source is the host's; no interface returns a value. |
| The journal (`p1-journal`) | The session record is native and is the single truth (ADR-0021), including the assembly identity of ADR-0080. |

`p1-core` still depends only on `p1-contracts`, and the WebAssembly runtime lives in the host:
wasmtime, the loader, the executor and the per-contract adapters are in `p1-module-runtime`,
never in the core. The core's view of a module is an ordinary `p1_contracts` trait object
constructed at the host's one composition root, exactly as ADR-0002 and ADR-0004 require; what
changed is one layer below the composition root, where the module is no longer linked in.

A guest has no WASI surface and no std I/O by design. The guest target is
`wasm32-unknown-unknown` (`modules/toolchain.pins`), componentized by the build with
`wasm-tools component new`; a component with any `wasi:` import is refused, not allow-listed
(D-XO-4). A guest panic is the wasm `unreachable` trap, which the host maps through
`ModuleFailure` into the existing closed shapes — `ToolOutcome` for a tool call and
`ProviderErrorKind` for a provider outcome — and adds no kind ([`protocol.md`](../protocol.md)).

A guest crate may use only serde, serde_json and regex (D-XO-8); any other crate is a
programme question, not a judgment call. Guest logic that belongs to more than one build target
may live in a target-independent library crate under `crates/`, shared by the native adapter's
tests and the component package (S0-R3, [`package.md`](../package.md)).

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
  ([`protocol.md`](../protocol.md)).
- Let a guest use any crate (as a native crate may): rejected by D-XO-8's three-crate bound.

## Evidence

- The host runtime crate exists with the bounded dependency set of Q7: `p1-module-runtime` on
  wasmtime 49.0.1, component-model and async features, no `cache` and no WASI, merged as PR
  #218 (merge commit `89922da2`).
- The closed error mapping and the JSON schema bundle are published: PR #209 (merge commit
  `07881e99`), the `p1-module-protocol` crate of [`protocol.md`](../protocol.md).
- The module workspace and the first package carry the guest rules: `modules/Cargo.toml`'s
  `unsafe_code = "forbid"` and release profile with `panic = "abort"`, merged as PR #240 (merge
  commit `a64d9e76`), with [`package.md`](../package.md).
- The WIT worlds give a guest no `wasi:` interface to import: PR #226 (merge commit `2f9c2223`)
  and PR #245 (merge commit `36ec6e16`), with [`wit.md`](../wit.md).
- The loader and executor that put the runtime in the host, the `wasm32-unknown-unknown` target
  of D-XO-4 and the S0-R3 reading of guest crates land with S0.5 (PR #252, open at drafting) and
  are recorded here when they merge.
- The remaining evidence — the S0.5–S0.8 PRs and their definition-of-done rows — is added before
  the `wasm-boundary-v1` tag. The recorded DoD runs are the evidence bundle
  `.wasm/up/evidence/` on box `wasm-s0`.
