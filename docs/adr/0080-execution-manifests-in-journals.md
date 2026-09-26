---
adr: 80
title: Execution manifests in journals
status: proposed
date: 2026-09-25
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0021, ADR-0049, ADR-0071, migration plan finding F3, freeze item 7]
---
# ADR-0080: Execution manifests in journals

## Context

ADR-0021 makes the session journal the single truth: a JSONL file with a
`{"p1_journal":1}` header, dense `seq` records, and an unknown version refused rather
than guessed. This decision amends ADR-0021 in one sentence only: the one that names
`{"p1_journal":1}` as the header of a journal the store creates. ADR-0021 stays
accepted and is cited here, not superseded; nothing else in it changes and no
`superseded_by` link is made, so its record is not edited.
ADR-0049 re-commits the `Environment` record whenever a session switches
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

S1.9 fixed what the six fields mean, without a format version bump, because every value
below fits a field the record already had:

- `kind` is the class the module is assembled into the agent as; the two `workflow-*`
  manifest kinds are never assembled into an agent, so four suffice;
- `name` is the module's own identity: a package's manifest `name` (`p1/<name>`), the crate
  that provides a native tool (`p1-tool-read`), or the manifest name of the component that
  will replace a native policy (`p1/context/summarizing`, `p1/policy/ask`);
- `package` is the key the environment selects the module by: the `modules.lock` key of a
  package, or the catalog key of a native module;
- `version` is the release version the lock pins for a package, and the p1 binary's own
  version for a native module;
- `digest` is the loader-verified digest in BARE lowercase hex (the manifest spelling is
  `sha256:<hex>`; the writer strips the prefix), and `null` for a native module, whose code
  the host's `commit` identifies;
- `abi` is `<world>+<protocol>` (e.g. `p1:module/tool@1.0.0+1.0`), and `null` for a native
  module, which has no WIT world.

Grants are deliberately not carried: the digest pins the manifest that grants them, so the
same bytes cannot gain a grant. The loader-built `variant` is not carried either: the
manifest `name` is the package that holds it.

`JsonlJournal::record_assembly(&AssemblyIdentity)` writes the line with the durability of a
record commit under the store's `SyncPolicy`; `MemoryJournal::record_assembly` mirrors it.
`Loaded` and `Resumed` carry `version: u64` and `assemblies: Vec<AssemblyEntry { from_seq,
identity }>`, `from_seq` being the seq of the first record after the line. A torn
assembly line is a truncated tail like a torn record. An assembly line in a version-1 file
is `JournalError::AssemblyInVersion1 { line }`; `record_assembly` on a version-1 file is
`JournalError::AssemblyNeedsVersion2`. The host arms the line for the assembly running now
and writes it as the run commits its first record — the `Environment` record comes first in a
turn, and a resume whose first request is refused commits nothing at all, so such a resume
writes nothing — and again whenever the assembly changes (model switch, reconfiguration); that
writer and the resume comparison land with slice S1.9, in `p1_host::run`.

On resume the host compares the journal's last assembly identity with the identity it
assembled now and prints a changed-artifact report: one line per module whose digest,
package or version changed, plus every module added or removed, and the host and the
environment when they differ. A changed artifact never blocks the resume — the journal's
claim is reported, never taken silently. A version-1 file carries no line, so a resume over
it reports nothing and writes none.

## Consequences

- A replayed session can report which modules changed since it was written (the
  changed-artifact report) instead of trusting names.
- Old binaries cannot resume sessions written by new ones; that refusal is the point.
- `p1-journal`'s `Loaded` and `Resumed` gain the assembly entries; their in-repo
  consumers adapt in the same PR (plan §6 mechanical adaptation).
- The workflow run journal is a separate format and is unchanged.
- ADR-0021 is amended in part, not superseded: it stays `accepted` and gains no
  `superseded_by` link, and only its `{"p1_journal":1}` header sentence is read as
  amended by this decision's version 2. `docs/adr/0021-*.md` is left unchanged.

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
journal_identity` (S1.9: the version-2 fixture the host writes, the loader-verified digest
and ABI in the line, a resume over an unchanged assembly, a swapped package's bytes, an
added and a removed module, a version-1 journal with no line and no report, and the
released binaries' header check); `scripts/gate.sh` on the S1.9 branch. The pinned release
binary itself (`p1 0.0.1 (329e3537f38f 2026-09-25)`, built before version 2 existed) refuses
the version-2 file the test writes, verbatim in the S1.9 PR body. PRs and merge commits are
cited when this ADR is flipped to accepted.
