---
adr: 92
title: Call-scoped capability services and a hostcall budget sized for whole files
status: accepted
date: 2026-09-27
deciders: lead
supersedes: []
superseded_by: []
sources: [PR #386 Codex review (crates/p1-tool-read/src/capability.rs, crates/p1-module-runtime/src/capabilities.rs), PR #386 comments (XO 2026-09-27 01:44, review-fix 02:25), docs/adr/0082-component-abi-and-execution-ownership.md, docs/adr/0088-workspace-capabilities-across-components.md, docs/design/modules/workspace-mutation.md, docs/design/modules/cancellation.md]
---
# ADR-0092: Call-scoped capability services and a hostcall budget sized for whole files

## Context

S2 activates the `p1/edit`, `p1/write`, `p1/patch` and `p1/search` components (issue #375,
PR #386). Codex's review of the PR found two defects in the frozen module runtime
(`p1-module-runtime`, ADR-0082), and the owner's comment on the PR asked for a runtime fix or an
explicit decision before landing.

1. **The read record was shared by every call of one assembled tool.** ADR-0088 point 3 and
   `docs/design/modules/workspace-mutation.md` step 3 give the gated recheck the identity of what
   *this call* read. The runtime, though, links `Services` once per assembly, and each call's
   Store only copies them (`CallState::new`). `p1_tool_read::tool_services` built one
   `ReadRecord` per assembly, so a later call inherited an earlier call's read. A patch that
   recreated a path an earlier call had read and moved away was refused as "changed on disk".
   Worse, two concurrent calls overwrote each other's digest, so a change computed from old
   contents could pass the recheck.
2. **Whole files above about three megabytes trapped.** A component hands the host whole files
   (`snapshot.check`, `snapshot.observe`, `workspace-mutation` `write` and `create`). The
   imports are linked dynamically: wasmtime's typed forms need derived `ComponentType`, which
   expands to `unsafe impl`, and the workspace forbids unsafe code. A dynamic lift charges one
   `Val` (about forty bytes) of *hostcall fuel* per list element, and wasmtime's default budget
   is 128 MiB per import call, so any file above about three megabytes trapped with "fuel
   allocated for hostcalls has been exhausted". The native tools have no such limit, and the
   oracle's 4 MiB atomic-replacement case failed through the write component.

## Decision

1. **Call-scoped services.** `Services` gains `call_scope`, an optional builder called once at
   the start of every export call, before its Store (`CallState::new`). Each service it returns
   serves that call in place of the assembly's. The assembly's fields are still what the linker
   checks against the manifest, so a scope returns a service only where the assembly holds one.
   `Services::call_scoped(build)` builds the assembly's services with the same builder.
   `p1_tool_read::tool_services` and `capability_services` are call-scoped: each call gets its
   own read side, open-snapshot cache and mutation service over one fresh `ReadRecord`. The host
   hook that merges a family's member services with the base (`locked_module_services`) carries
   the base's `workspace-mutation` and `call_scope` as well as `workspace` and `snapshot`.
2. **A hostcall budget sized for whole files.** Every call's Store gets
   `HOSTCALL_FUEL = MAX_TRANSFER_BYTES × size_of::<Val>() + 128 MiB`, with
   `MAX_TRANSFER_BYTES = 16 MiB` (`p1_module_runtime::executor`). The workspace sets no file
   size of its own, so 16 MiB is the largest file a component can check, edit, write or patch.
   One import call can still make the host allocate only a bounded amount, now about
   `HOSTCALL_FUEL` bytes (roughly 0.8 GiB) instead of 128 MiB.

## Consequences

- One call's read never satisfies or blocks another call's mutation, sequential or concurrent.
  Two cases in `crates/p1-module-tests/tests/filesystem_tools.rs` hold this, and
  `every_call_gets_its_own_call_scoped_services` holds the runtime hook.
- A test or host step that wraps one of a call-scoped assembly's services must wrap it inside
  the scope, or every call is served the unwrapped one. The two S2 suites that wrap the mutation
  service do this.
- Files up to 16 MiB go through the components. The oracle's 4 MiB case passes, and the S2
  atomicity case runs at the oracle's 4 MiB again. A larger file through a component still
  traps, naming the hostcall fuel, where the native tool succeeds. Raising the limit means
  changing one constant and accepting the host allocation that comes with it (about forty bytes
  per byte transferred) on a machine with 7 GB of RAM.
- The budget applies to every executor call (tools and context policies), not only the
  workspace imports. No other import passes a list anywhere near this size.

## Alternatives considered

- **Typed whole-buffer linking** (`func_wrap_async` with `Vec<u8>` parameters, one byte of fuel
  per byte). This is the brief's preferred option, and it would cut the host allocation about
  forty times. The results are `result<_, fs-error>` (and `result<snapshot-observation,
  fs-error>`), and a typed host function needs `Lower` for the `fs-error` variant. Wasmtime
  provides that only through the `ComponentType`/`Lift`/`Lower` derives, which emit
  `unsafe impl`s that `unsafe_code = "forbid"` refuses. Lifting typed parameters while lowering
  dynamic results is not an API wasmtime 49 offers. Reversing the unsafe rule is an owner
  decision, not this slice's.
- **Services built per call by the tool adapter** (`wasm_tool` taking a factory instead of
  `Services`). This changes every caller of `wasm_tool` and every hook's type. The optional
  scope keeps both, and non-scoped services behave as before.
- **A per-call record behind a task-local or a call identifier passed to every service
  method.** This is either hidden global state or a change to every service trait.

## Evidence

- `cargo test -p p1-module-runtime every_call_gets_its_own_call_scoped_services`.
- `cargo test -p p1-module-tests --test filesystem_tools` (after `scripts/build-modules.sh
  --all`): `a_later_call_does_not_inherit_an_earlier_calls_read`,
  `concurrent_calls_of_one_tool_keep_their_own_read_identity`, and
  `a_component_replacement_is_never_observed_partially` at 4 MiB.
- `cargo test -p p1-host a_family_hook_keeps_the_mutation_and_the_call_scope_of_a_non_member`.
- The sealed S2 oracle through the component factory: `u_mut::atomic_replacement_no_partial_read`
  (4 MiB) passes. The result lines are in PR #386's body.
- wasmtime 49.0.1: `Store::set_hostcall_fuel` (default 128 MiB) and `consume_fuel_array(len,
  size_of::<Val>())` in `runtime/component/values.rs`.
