<!-- ADR draft: numbered at landing with scripts/adr.py new "Component ABI and execution ownership" --deciders owner+lead -->
sources: [DECISIONS.md D22, docs/adr/0015-send-capable-boxed-future-contracts.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0079-verified-module-releases-and-installation.md, docs/design/modules/protocol.md, docs/design/modules/wit.md, docs/design/modules/package.md, crates/p1-module-runtime/src/lib.rs]
# Component ABI and execution ownership

## Context

ADR-0015 makes p1's public async contracts return boxed `Send` futures and gives each agent's
mutable state a single owner; the core spawns no tasks and needs no particular runtime flavour.
ADR-0071 names ADR-0015 among the ADRs the migration must revisit, because the two sides of a
WebAssembly boundary run on different clocks: a component's exports are plain synchronous calls
the host makes on the module's executor, while a host import may suspend the guest and await the
native host. The boundary therefore needs a rule for who owns a Store, how a synchronous export
becomes an awaitable call, and where the limits on a guest call live.

Two further constraints come from the contracts themselves. `p1_contracts::Tool`'s `effect` and
`describe` are synchronous methods the host calls from inside async code on any Tokio flavour, so
they must never wait on the async executor or block on a runtime. And the boundary carries rich
values — tool calls and outcomes, history items, stream events, provider errors, descriptions,
model options, route descriptions, replay payloads — which the WIT package cannot type without
locking `p1-contracts`' public shapes into the ABI.

The identity and source of a module are the other half of the same boundary. ADR-0071 ships
modules from p1's own release archive; ADR-0079 makes the release manifest authoritative. The
loader must not become a second, unchecked way to run code, and a module must not be able to
claim an identity or a grant it was not given. Finally, a component's imports are its capability
surface: what it may reach must be a static fact of its bytes, not a promise in its manifest.

## Decision

**ADR-0015 is preserved at the boundary.** Every world exports synchronous functions the host
calls on the module's one executor, and every host import is asynchronous: wasmtime's async host
functions suspend the guest while the native host awaits, so waiting for a process, a worker, a
summary or the write gate is an ordinary import call. No world uses WIT `future` or `stream`
types ([`wit.md`](../wit.md)).

**One Store-owning executor per module, behind a channel.** Callers never hold a Store. They send
a request with a reply channel and await the reply, so every public async method is a `Send`
boxed future whatever the caller's Tokio flavour; the executor task owns the module's Stores and
runs each call as its own task. Each call gets a fresh Store and a fresh instance, so a trap, a
deadline or an abandoned call poisons nothing for the next one, and dropping the store drops
everything the call held. **Fuel and an epoch deadline are set per call**: fuel bounds pure
computation, the epoch deadline bounds wall time while guest code runs, and a cancelled call gets
a grace window in which cooperative cancellation (`control.cancelled()`) can return cleanly.

**The restricted synchronous path for inspection.** The tool's `effect` and `describe` (and
`describe-result`, the other synchronous inspection export) run on a second instance of the
module with **no capability linked** — every import the linker defines is a trap — under a tight
fuel budget, called synchronously on the caller's own thread behind a mutex. The instance is
rebuilt after any trap, and a failed inspection degrades safely (a failed `effect` is `Executes`,
a failed `describe` is the empty `call` description). This is how PR #252 implements it: the
restricted Store links nothing asynchronous, so the same engine serves both paths and the call
uses wasmtime's synchronous entry point with no fiber, no future and no poll loop. It lands with
S0.5 (PR #252). The cancellation and streaming cases are S0.6's and exercise the executor's
execute path — epoch and fuel limits, cooperative `control.cancelled()` and the `process.running`
streaming resource — not this restricted inspection path.

**Rich values cross as JSON text under the protocol version rule.** Each value family has a
string alias in the WIT `types` and must conform to its schema, `p1:protocol/<family>/1`, where
the major is `PROTOCOL_VERSION`'s major in `p1-module-protocol`; a host refuses a module built
for another major, and a minor change is one an older host refuses cleanly. Closed objects reject
unknown fields on the wire and in serde. Small closed values the host reads without parsing JSON
— `effect`, `decision`, the declaration, `tool-identity`, `stop-reason` — are WIT types instead.

**Verify then compile the same bytes; digest is identity; official source only.** The loader
reads the component bytes once, hashes them, compares the digest with the release manifest of
ADR-0079 and only then compiles *those* bytes from memory; `Component::deserialize*` is never
called and no compiled cache is built, so the digest check is the whole trust decision. There is
no API that loads a path or bytes the caller chose: a name outside the reserved `p1/` namespace,
a name absent from the manifest, a digest mismatch, an unknown kind, a world or protocol major
the runtime does not speak, a capability it cannot link and any import the manifest does not
grant are each refused with a typed error. A module's `ToolIdentity` is **built by the loader**
from the package identity and variant, never reported by the module about itself
([`package.md`](../package.md)).

**The capability allocation is frozen, and the check is static.** Each module class has a frozen
allocation of capabilities ([`wit.md`](../wit.md)); the manifest narrows it, the host links only what
the manifest grants, and `scripts/check-module-boundaries.sh` compares a component's actual
imports against the allocation. Worker functions are split per member
(`workers-start`/`-observe`/`-control`) exactly so that what a module may do is visible in its
imports rather than in a per-function grant.

## Consequences

- ADR-0015 holds unchanged: the core and the adapters see `Send` boxed futures, one owner per
  Store, and no `async-trait`.
- A trap, a deadline, a fuel exhaustion or an abandoned call can never affect a later call, at
  the cost of one Store and one instance per call.
- Inspection cannot reach a capability by construction, so a module cannot use `describe` to
  read a file or spawn a process; what only a capability could know (a symlink escape) is judged
  lexically on the restricted path and enforced again when the call executes.
- The JSON text boundary keeps `p1-contracts`' public shapes out of the ABI, at the cost of a
  parse and serialize on each crossing; the version rule makes a mismatched pair fail at load or
  on a refused field rather than silently.
- Identity, source and grants are facts the host establishes, not claims the module makes.
- A component that imports a capability its manifest does not grant, or any `wasi:` interface,
  is refused rather than linked.
- The boundary needs no new error kind: what fails becomes `ModuleFailure` and maps into the
  closed `ToolStatus` and `ProviderErrorKind` ([`protocol.md`](../protocol.md)).

## Alternatives considered

- Depend on the async executor from the synchronous contract methods (locking, blocking or
  `block_on`): rejected; it would deadlock on a current-thread runtime, which is why the
  restricted path exists.
- Call the restricted exports through `call_async` polled with a no-op waker: rejected as PR
  #252 records; it is correct only while nothing on the path can be pending, an invariant a later
  change could break silently into a spin, whereas the synchronous call makes wasmtime refuse
  such a change loudly.
- Share one Store or cache instances across calls: rejected; a trap leaves an instance that may
  not be entered again, and per-call instances are what make cancellation and abandonment safe.
- Use WIT `future`/`stream` types or `async-trait` at the boundary: rejected; no world uses them
  and ADR-0015 rules out `async-trait`.
- Change a `p1-contracts` public type to type the rich values: rejected; the JSON families carry
  them without a public type change (the one lossy mapping is the call-verb vocabulary,
  [`protocol.md`](../protocol.md)).
- Load a module from a path or from bytes the caller chose, or accept a text-format module:
  rejected; only the release manifest is a source, and only binary components are compiled.
- Let the module report its own identity or self-declare its capabilities: rejected; both are
  host-built, and the import check compares bytes.

## Evidence

- The closed shapes and the version rule are published and tested: PR #209 (merge commit
  `07881e99`), `p1-module-protocol`, [`protocol.md`](../protocol.md).
- The worlds state synchronous exports and asynchronous imports, the restricted path and the
  per-class allocation: PR #226 (merge commit `2f9c2223`) and PR #245 (merge commit `36ec6e16`),
  [`wit.md`](../wit.md).
- The package format, the digest-as-identity rule and the loader-built `ToolIdentity` are
  published: PR #240 (merge commit `a64d9e76`), [`package.md`](../package.md).
- The runtime crate the executor and loader live in is merged with the bounded dependency set:
  PR #218 (merge commit `89922da2`).
- The loader (verify-then-compile-same-bytes, official source only), the one Store-owning
  executor behind a channel with boxed `Send` futures, the per-call Store, instance, fuel and
  epoch deadline, the restricted synchronous path as described above and the `WasmTool` adapter
  are implemented by PR #252 (S0.5, open at drafting) and are recorded here when they merge.
- The remaining evidence — the S0.5–S0.8 PRs and their definition-of-done rows — is added before
  the `wasm-boundary-v1` tag. The recorded DoD runs are the evidence bundle
  `.wasm/up/evidence/` on box `wasm-s0`.
