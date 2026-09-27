---
adr: 82
title: Component ABI and execution ownership
status: accepted
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [DECISIONS.md D22, docs/adr/0015-send-capable-boxed-future-contracts.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0079-verified-module-releases-and-installation.md, docs/design/modules/README.md, docs/design/modules/protocol.md, docs/design/modules/wit.md, docs/design/modules/package.md, docs/design/modules/cancellation.md, docs/design/modules/adapters.md, crates/p1-module-runtime/src/lib.rs, crates/p1-module-runtime/src/executor.rs, crates/p1-module-runtime/src/restricted.rs, crates/p1-module-runtime/src/loader.rs, crates/p1-module-runtime/src/tool.rs]
---
# ADR-0082: Component ABI and execution ownership

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
types ([`wit.md`](../design/modules/wit.md)).

**One Store-owning executor per module, behind a channel.** Callers never hold a Store. They send
a request with a reply channel and await the reply, so every public async method is a `Send`
boxed future whatever the caller's Tokio flavour; the executor task owns the module's Stores and
runs each call as its own task. Each call gets a fresh Store and a fresh instance, so a trap, a
deadline or an abandoned call poisons nothing for the next one, and dropping the store drops
everything the call held. **Fuel and an epoch deadline are set per call**: fuel bounds pure
computation, the epoch deadline bounds the whole call's wall time, host waits included, and a
cancelled call gets a grace budget of fuel in which cooperative cancellation
(`control.cancelled()`) can return cleanly. A trap never undoes a native effect
([`cancellation.md`](../design/modules/cancellation.md)).

**The restricted synchronous path for inspection.** The tool's `effect` and `describe` (and
`describe-result`, the other synchronous inspection export) run on a second instance of the
module with **no capability linked** — every import the linker defines is a trap — under a tight
fuel budget and an epoch backstop, called synchronously on the caller's own thread behind a
mutex. The instance is rebuilt after any trap, and a failed inspection degrades safely (a failed
`effect` is `Executes`, a failed `describe` is the empty `call` description). The restricted Store
links nothing asynchronous, so the same engine serves both paths and the call uses wasmtime's
synchronous entry point with no fiber, no future and no poll loop; this is what PR #252 (merge
commit `4872fd78`) implements in `crates/p1-module-runtime/src/restricted.rs`. The cancellation
and streaming cases of PR #272 (merge commit `a808a2ed`) exercise the executor's execute path —
epoch and fuel limits, cooperative `control.cancelled()` and the `process.running` streaming
resource — not this restricted inspection path.

**Rich values cross as JSON text under the protocol version rule.** Each value family has a
string alias in the WIT `types` and must conform to its schema, `p1:protocol/<family>/1`, where
the major is `PROTOCOL_VERSION`'s major in `p1-module-protocol`; a host refuses a module built
for another major, and a minor change is one an older host refuses cleanly. Closed objects reject
unknown fields on the wire and in serde. Small closed values the host reads without parsing JSON
— `effect`, `decision`, the declaration, `tool-identity`, `stop-reason` — are WIT types instead
([`protocol.md`](../design/modules/protocol.md)).

**Verify then compile the same bytes; digest is identity; official source only.** The loader
reads the component bytes once, hashes them, compares the digest with the release manifest of
ADR-0079 and only then compiles *those* bytes from memory; `Component::deserialize*` is never
called and no compiled cache is built, so the digest check is the whole trust decision. There is
no API that loads a path or bytes the caller chose: a name outside the reserved `p1/` namespace,
a name absent from the manifest, a file that is not a regular file, a digest mismatch, an unknown
kind, a world or protocol major the runtime does not speak, a capability it cannot link and any
import the manifest does not grant are each refused with a typed error. A module's `ToolIdentity`
is **built by the loader** from the package identity and variant, never reported by the module
about itself ([`package.md`](../design/modules/package.md)).

**The capability allocation is frozen, and the check is static.** Each module class has a frozen
allocation of capabilities (`modules/capabilities.toml`,
[`capabilities.md`](../design/modules/capabilities.md)); the manifest narrows it, the host links
only what the manifest grants, and `scripts/check-module-boundaries.sh` compares a component's
actual imports against the allocation. Worker functions are split per member
(`workers-start`/`-observe`/`-control`) exactly so that what a module may do is visible in its
imports rather than in a per-function grant.

**One generic adapter per contract, in the runtime crate.** A tool component is adapted to
`p1_contracts::Tool` by `WasmTool` alone, and the host only receives it wrapped in
`RedactingTool`; streams implement components, not adapters. The provider adapter is S4's and
the context and authorization policy adapters are S5's, each in its own file of
`p1-module-runtime` on this ADR's executor and restricted path
([`adapters.md`](../design/modules/adapters.md)).

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
  closed `ToolStatus` and `ProviderErrorKind` ([`protocol.md`](../design/modules/protocol.md)).

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
  [`protocol.md`](../design/modules/protocol.md)).
- Load a module from a path or from bytes the caller chose, or accept a text-format module:
  rejected; only the release manifest is a source, and only binary components are compiled.
- Let the module report its own identity or self-declare its capabilities: rejected; both are
  host-built, and the import check compares bytes.

## Evidence

The published freeze is indexed in [`docs/design/modules/README.md`](../design/modules/README.md).
Every S0 PR, with its merge commit on main and the main `gate` run of that commit:

- S0.1, PR #205, merge commit `0c359b8d` (`0c359b8df904acdc9badb1f21b9fcca9b8758ed5`): the module
  toolchain check and the gate's module hook the later PRs ran under; main gate run
  36160382357, success.
- S0.3.1, PR #209, merge commit `07881e99` (`07881e99f5c26e5fe8e27ed2d2e608ee217e7fa7`): the closed
  shapes, the JSON families and the version rule, tested in `p1-module-protocol`
  ([`protocol.md`](../design/modules/protocol.md)); main gate run 36166930390, success.
- S0.2, PR #218, merge commit `89922da2` (`89922da286baac96758e9722872776fdc3b6c509`): the runtime
  crate the executor and loader live in, with the bounded dependency set and no `cache` feature;
  main gate run 36173616397, success.
- S0.3.2, PR #226, merge commit `2f9c2223` (`2f9c2223b42c0ff703417c32b9ea108ff38f0f59`): the worlds
  with synchronous exports and asynchronous imports, the restricted path and the per-class
  allocation ([`wit.md`](../design/modules/wit.md)); its own main gate run 36176731212 was
  cancelled by the concurrency group and is covered by the descendant `e850cb88` run
  36176937286, success.
- S0.4, PR #240, merge commit `a64d9e76` (`a64d9e7607ebc3437f8f9c003972490fc665e239`): the package
  format, the digest-as-identity rule and the loader-built `ToolIdentity`
  ([`package.md`](../design/modules/package.md)); main gate run 36188665897, success.
- S0.3.3, PR #245, merge commit `36ec6e16` (`36ec6e1626154989d54222a94c1e33353e41a88a`): the worker
  interface split that keeps per-member grants static (S0-R1.3) and the other S0-R1 and S0-R2
  amendments; main gate run 36190044937, success.
- S0.5, PR #252, merge commit `4872fd78` (`4872fd780581c39cc52df068b197ab2573a28e62`): the loader
  (verify-then-compile-same-bytes, official source only), the one Store-owning executor behind a
  channel with boxed `Send` futures, the per-call Store, instance, fuel and epoch deadline, the
  restricted synchronous path and the `WasmTool` adapter; `cargo test --locked -p
  p1-module-tests --test runtime_spike` passes on current-thread and multi-thread Tokio; main
  gate run 36199422718, success.
- S0.7, PR #270, merge commit `e8682c7e` (`e8682c7e8d2f98e64fb4159cfdef53c61752fe38`): the frozen
  allocation as data and the static import check
  ([`capabilities.md`](../design/modules/capabilities.md)); main gate run 36207266519, success.
- S0.6, PR #272, merge commit `a808a2ed` (`a808a2ed369fb4d94133632feb3d5fb0eef45b80`): epoch
  deadline, fuel, cooperative cancellation, the grace fuel and the streaming-resource contract;
  `cargo test --locked -p p1-module-tests --test cancellation`
  ([`cancellation.md`](../design/modules/cancellation.md)); its own main gate run 36208468329
  was cancelled by the concurrency group and is covered by the descendant `acbe4304` run
  36208581179 (S0.8's), success.
- S0.8, PR #271, merge commit `acbe4304` (`acbe43049258c3fbc1e045bdbc393b5d855b388b`): the cold and
  warm cost of the executor and the restricted path recorded as facts
  ([`baseline.md`](../design/modules/baseline.md)); main gate run 36208581179, success.
- S0.9, PR #269 (issue #250): this ADR, the index and the remaining freeze documents.
- Acceptance: this ADR is accepted with slice S0.9 on the `judge` role's ACCEPT of that slice;
  the tag `wasm-boundary-v1` is cut on S0.9's merge commit only after it.
- The recorded definition-of-done runs, gate logs and verdicts are the evidence bundle
  `.wasm/up/evidence/` on box wasm-s0.
