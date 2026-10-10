---
adr: 160
title: Sandbox write grants are shared file-tool roots
status: accepted
date: 2026-10-10
deciders: lead
supersedes: []
superseded_by: []
sources: [issue #84, issue #70, ADR-0025, ADR-0122]
---
# ADR-0160: Sandbox write grants are shared file-tool roots

## Context

Issue #84 reports a sandboxed review that could write its output directory through
the shell but not through edit/write. The host passed `--sandbox-write` to the
process sandbox, while file services received only the workspace and scratch roots.
Issue #70 asks for one shared implementation of file-tool safety and previews.

## Decision

The host passes sandbox write grants through the assembly catalog to every agent's
`p1-workspace::Workspace`. Each grant is a canonical existing directory, admitted
as a confined root for both reads and mutations. Native tools and module capability
services share this workspace; no new WIT interface or permission switch is added.

Read-before-mutate, stale-file checks, credential refusal, descriptor walks and the
write gate remain unchanged. A symlink may not escape its owning root. Scratch
retains its existing ownership; paths inside the workspace retain workspace mutation
accounting even if also granted. External overlapping grants use the most specific
root. Refused paths name the configured writable roots when grants exist.

## Consequences

An output directory explicitly granted to the sandbox is usable by the read, edit
and write tools, including workspace-relative spellings that resolve into it.
Without grants, the existing confinement and error texts remain unchanged. Granted
directories missing at assembly time are inactive, matching the process sandbox's
existing missing-bind behavior. Existing grants must be directories. The frozen payload-free WIT escape error
cannot name roots, so with grants the capability service uses its existing text-bearing
`io` error case for the detailed refusal. Tool schemas and mutation semantics do not change.

The native preview conversion lives in `p1-workspace`; tool-specific parsing,
replacement logic and summaries remain in each tool's logic crate.

## Alternatives considered

Passing roots only to the native tools would leave the shipped WebAssembly tools
broken. Relaxing confinement globally would permit ungranted writes. A new WIT
error payload would unnecessarily change the frozen module boundary.

## Evidence

`crates/p1-host/tests/sandbox.rs::sandbox_write_reaches_the_shipped_file_tools`
scripts the reported job's shape against scratch fixtures: create a verdict with
the shipped write tool, read it, then edit it through a relative path. The same
calls without a grant are refused, and a sibling outside all roots is refused in
both cases. `crates/p1-workspace/tests/scratch.rs` covers multiple grants,
read-before-mutate, stale observations, dot-dot/symlink escapes and nested-grant
mutation accounting. Existing tool tests remain unchanged.
