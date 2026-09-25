---
adr: {{number}}
title: Execution manifests in journals
status: proposed
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0021, ADR-0049, ADR-0071, migration plan finding F3, freeze item 7]
---
# ADR-{{number_padded}}: Execution manifests in journals

## Context

ADR-0021 makes the session journal the single truth: a JSONL file with a
`{"p1_journal":1}` header, dense `seq` records, and an unknown version refused rather
than guessed. ADR-0049 re-commits the `Environment` record whenever a session switches
model. Until now the code that executed a tool call was fixed at compile time, so the
binary's commit named it.

Under ADR-0071 the host loads tools, providers and policies as WebAssembly packages by
name. The same binary can run different package bytes, so a journal that records only
names cannot say what executed a call, and replay against changed packages would pass
silently. Finding F3 of the migration review: the journal must carry the execution
manifest. Freeze item 7: a version-bearing record old binaries fail on, and the
assembly identity record.

## Decision

The journal format moves to version 2. New journals (`JsonlJournal::create`) start with
`{"p1_journal":2}`; the released binaries accept only version 1 and refuse the file with
"unknown journal version", so an old binary never replays a journal whose execution it
cannot check. New p1 reads versions 1 and 2 (`load`, `resume`, `open_for_append`); any
other version stays `JournalError::UnknownVersion`. Appending to a version-1 file keeps it
at version 1: its header is never rewritten.

Version 2 adds one line kind beside the `seq` records: an assembly identity line
`{"assembly":{...}}`, owned by `p1-journal` and not a `p1-contracts` record, so the core
and its `CommitSink` are unchanged. The line carries no `seq`, does not disturb the
dense-seq rule, and applies to the records that follow it until the next assembly line.
Its payload is `p1_journal::AssemblyIdentity { environment, host: HostIdentity { version,
commit }, modules: Vec<ModuleIdentity> }`, each `ModuleIdentity { name, kind: ModuleKind,
package, version, digest: Option<String>, abi: Option<String> }` where `digest` is the
sha256 hex of the package bytes the loader verified (`None` for a native module) and
`ModuleKind` is one of `tool`, `provider`, `context_policy`, `authorization_policy`. Every
type denies unknown fields, so an extension is a version bump, never a silent drop.

`JsonlJournal::record_assembly(&AssemblyIdentity)` writes the line with the durability of a
record commit under the store's `SyncPolicy`; `MemoryJournal::record_assembly` mirrors it.
`Loaded` and `Resumed` carry `version: u64` and `assemblies: Vec<AssemblyEntry { from_seq,
identity }>`, `from_seq` being the seq of the first record after the line. A torn
assembly line is a truncated tail like a torn record. An assembly line in a version-1 file
is `JournalError::AssemblyInVersion1 { line }`; `record_assembly` on a version-1 file is
`JournalError::AssemblyNeedsVersion2`. The host writes one line before the first
`Environment` record and again whenever the assembly changes (model switch,
reconfiguration); that writer lands with slice S1.9, which builds the identity in the
loader.

## Consequences

- A replayed session can report which modules changed since it was written (the
  changed-artifact report) instead of trusting names.
- Old binaries cannot resume sessions written by new ones; that refusal is the point.
- `p1-journal`'s `Loaded` and `Resumed` gain the assembly entries; their in-repo
  consumers adapt in the same PR (plan §6 mechanical adaptation).
- The workflow run journal is a separate format and is unchanged.

## Alternatives considered

- A new `RecordBody` variant in `p1-contracts`: rejected for now; it changes the core's
  public record type and every consumer, and the contracts group is frozen by S0.
- Carrying the identity inside the `Environment` record: same objection, and it would
  repeat package metadata on every model switch that changes no module.
- Keeping version 1 and adding an optional line: rejected; an old binary would ignore
  or misparse it instead of refusing the file.

## Evidence

`cargo test --locked -p p1-journal` (version 1 and 2 read, version 2 written, an unknown
version refused, assembly round trip); `cargo test --locked -p p1-module-tests --test
journal_identity` (S1.9: old and new fixtures, the changed-artifact report, the released
binary refusing a version-2 journal). PRs and merge commits are cited when this ADR is
flipped to accepted.
