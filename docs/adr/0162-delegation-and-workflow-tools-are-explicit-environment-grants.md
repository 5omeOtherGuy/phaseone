---
adr: 162
title: Delegation and workflow tools are explicit environment grants
status: accepted
date: 2026-10-10
deciders: lead
supersedes: [50]
superseded_by: []
sources: [issue #85, issue #75, docs/adr/0050-every-main-agent-can-start-workers-a-worker-gets-exactly-the-tools-its-parent-grants.md, docs/adr/0085-runtime-delegation-and-workflow-modules.md, crates/p1-host/src/catalog/delegation.rs]
---
# ADR-0162: Delegation and workflow tools are explicit environment grants

## Context

Issue #85 reports that a shell-only environment still receives delegation tools.
The host appended the worker and workflow families at all three parent assembly sites.
The lead approved reversing ADR-0050's injection default: an agent sees only its
assembled prompt and explicitly selected tools, not tools its prompt must forbid.

## Decision

Worker and workflow tools are assembled only when the environment lists their catalog
keys or module packages. Enabled capabilities permit selection; they do not add tools.
Naming a disabled member remains an assembly error, including verified aliases.

This supersedes ADR-0050 item 1 and ADR-0053 item 7's automatic workflow injection,
as carried forward by ADR-0085. ADR-0050 items 2–6 remain unchanged: delegation is
optional, prompts remain conditional, and children receive exactly their parent grant
plus finish. Shipped general-purpose environments list all eight members explicitly;
aliases inherit those lists. Leaf environments remain unchanged.

## Consequences

A shell-only environment sends only shell to the provider and journals that same grant.
Existing custom environments that relied on automatic delegation must list the members
they need. No new core or runtime service is introduced.

## Alternatives considered

Keep injection and forbid delegation in prompts: this leaves callable tools outside
the environment's grant. Add an opt-out flag: this duplicates the tool list and retains
an implicit default. Neither satisfies issue #85.

## Evidence

`crates/p1-host/tests/worker_grants.rs` exercises a shell-only run against a scripted
provider and checks both its request and environment journal record. The existing
child-grant tests verify exact child tools; catalog tests cover explicit member packages,
verified aliases and disabled capabilities. Recheck with `cargo test -p p1-host` after
`scripts/build-modules.sh --all`.
