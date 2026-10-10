---
adr: 155
title: JSON object order is explicit and preserve_order is rejected by the gate
status: accepted
date: 2026-10-10
deciders: lead
supersedes: []
superseded_by: []
sources: [https://github.com/5omeOtherGuy/phaseone/issues/689]
---
# ADR-0155: JSON object order is explicit and preserve_order is rejected by the gate

## Context

Cargo unifies serde_json features across dependencies. Enabling `preserve_order`
changes object iteration from sorted keys to insertion order, affecting first-error
selection, capped diagnostic lists, truncated displays and workflow prompt matching.
Issue #689 records the ACP dependency incident and the owner's protected-variation
requirement. The owner approved a serde_json-only leaf crate and this guard rule.

## Decision

`p1-json-order` owns recursive canonicalization, sorted object entries and compact
canonical JSON. Order-sensitive diagnostics and rendering use ascending lexicographic
object keys explicitly; arrays retain their order, including required-key lists.
The finish validator keeps the first 32 errors in its documented depth-first order.

The gate rejects `serde_json/preserve_order` in both workspaces: a p1-host integration
test inserts reverse-ordered keys and checks default map order, and a module feature-tree
check rejects the enabled feature. This reports dependency feature unification at its
source rather than allowing unrelated ordering failures or hung prompt-matching tests.

## Consequences

Existing sorted-map behavior remains unchanged. The helper is portable to WASM and
depends only on serde_json; it implements no provider, tool or runtime behavior.
Sorted traversal costs temporary entry storage. Cosmetic provider bodies, tool schemas,
credential-file rewrites and persisted JSON remain untouched.

A future dependency that requires preserve_order needs an explicit reversal of this
guard rule, even though the protected call sites work with either map backend.

## Alternatives considered

Relying only on serde_json defaults repeats the feature-unification failure. A guard
alone catches dependencies but does not protect the named contracts from backend
variation. Enabling preserve_order everywhere makes insertion order a global behavior
change rather than preserving current diagnostics and rendering.

## Evidence

`cargo test -p p1-json-order -p p1-workflow -p p1-tool-finish -p p1-github-guest`
passes both normally and with `--features serde_json/preserve_order` from the root
workspace. Tests cover nested objects, array order, lexicographic first errors and
the 32-error cap. `scripts/test_json_order.py` tests module guard acceptance,
rejection, unrelated features and cargo failure; `scripts/test_gate.py` verifies
the guard is mandatory in the gate. The native guard's on/off proof is recorded in
the implementation pull request.
