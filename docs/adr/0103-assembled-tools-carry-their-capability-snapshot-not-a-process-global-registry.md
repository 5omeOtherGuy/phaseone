---
adr: 103
title: Assembled tools carry their capability snapshot, not a process-global registry
status: accepted
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: ["docs/design/modules/capabilities.md"]
---
# ADR-0103: Assembled tools carry their capability snapshot, not a process-global registry

## Context

F1 binds each assembled tool to the capability snapshot of the verified generation that
loaded it (the package manifest's grants, keyed in the loader by identity and digest). The
first implementation recorded that binding in `bound_tools`, a process-global, pointer-keyed
registry in `crates/p1-host/src/catalog/capabilities.rs`, and `carries` consulted it.

An independent landing review (Codex P1 on PR #471) raised the registry as a blocker: the
project's architecture rules forbid "a global registry, auto-registration or DI framework",
and a table shared by address is also wrong for the F1 model itself. Two sessions may bind
one tool object, and a reload may bind a replacement while a running child still holds the
old object; state kept by address, outside the generation, cannot distinguish those cases.
Capability grants belong to a verified generation of an assembly, not to a process.

## Decision

The `p1_contracts::Tool` trait gains a defaulted `as_any(&self) -> Option<&dyn std::any::Any>`
returning `None`. At assembly time `bind_assembled` / `bind_tools` replace each tool with a
`BoundTool` wrapper that owns that generation's immutable `Capabilities` snapshot and
forwards every other `Tool` method; `carries` reads the snapshot by downcasting through
`as_any`. Re-binding unwraps any existing `BoundTool` first, so wrappers do not stack.

The package-declaration map (`package_declarations()`) stays only as the fallback lookup for
unbound fixtures and standalone native tools, not as the mechanism an assembled tool uses.

## Consequences

* An assembled tool carries its grants on the object: no process-wide table is consulted for
  a bound tool, two sessions that bind the same object share no state, and an old
  generation's tool keeps its snapshot after a reload.
* `as_any` is a new defaulted method on a public trait (an interface change). Implementors
  that do not override it keep the default `None`.
* A wrapper applied AFTER binding hides the snapshot: `carries` then finds no `BoundTool` and
  falls back to `declared`, which need not reflect the bound generation. No production call
  site wraps a bound tool today.
* The fallback map stays process-global for its remaining users, so it keeps the property the
  review objected to — but only where there is no generation to bind (test fixtures and
  standalone native tools).

## Alternatives considered

* **Keep the pointer-keyed registry and document it.** Rejected: the architecture rule
  forbids it and the review named it.
* **Make `carries` a method on `Tool` so each tool answers for itself.** That changes every
  implementor and freezes the answer into the tool rather than the assembly's verified
  snapshot; the wrapper keeps the snapshot on the object without changing implementors.
* **Store the snapshot in a per-assembly side table keyed by tool address but owned by the
  generation.** Rejected: it is the same address-keyed design with a shorter lifetime, and it
  still breaks when one object is bound by two sessions.

## Evidence

* `crates/p1-contracts/src/tool.rs`: the defaulted `as_any` on `Tool`.
* `crates/p1-host/src/catalog/capabilities.rs`: `BoundTool`, `unbound`, `bind`,
  `bind_assembled`, `bind_tools`, `carries`.
* `crates/p1-host/src/catalog/capabilities.rs` tests:
  `two_sessions_binding_one_tool_object_share_no_state`,
  `rebinding_replaces_the_snapshot_instead_of_stacking_wrappers`,
  `an_old_tool_keeps_its_bound_capabilities_after_a_replacement`.
