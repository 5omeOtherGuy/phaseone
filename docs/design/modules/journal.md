# Journal: the version record and the assembly identity

Status: published freeze item 7 of the WebAssembly boundary (ADR-0071, review finding F3): a
version-bearing record that old binaries fail on, and the assembly identity record. The journal
is native and stays native ([ADR-0081](../../adr/0081-native-foundation-and-runtime-components.md));
its code is S1's. This document publishes what that code does today — journal format version 2
with assembly identity lines, landed by S1.3 (PR #228, merge commit `6d6978f1`) under
[ADR-0080](../../adr/0080-execution-manifests-in-journals.md) — and lists what the boundary still
needs from it under "Gaps for S1". The store's full specification is
[`docs/design/journal.md`](../journal.md); the code is
[`crates/p1-journal/src/lib.rs`](../../../crates/p1-journal/src/lib.rs) and its tests are
[`crates/p1-journal/tests/journal_version.rs`](../../../crates/p1-journal/tests/journal_version.rs).

## Why the boundary needs it

Before the migration the code that executed a tool call was fixed at compile time, so the
binary's commit named it. Under ADR-0071 the same binary loads different package bytes by name,
so a journal that records only names cannot say what executed a call, and a replay against
changed packages would pass silently. The journal therefore has to name what executed each
record, and a binary that cannot check that must refuse the journal instead of replaying it.

## The version record

- **What it is.** The first line of every session file is a header naming the format version:
  `{"p1_journal":2}` for a file `JsonlJournal::create` writes (`JOURNAL_VERSION`),
  `{"p1_journal":1}` for a file written by the released binaries (`JOURNAL_VERSION_1`). It is
  the only line that is neither a record nor an assembly line, and it carries nothing else.
- **Where it sits.** Line 1, before any record; it is never rewritten. p1 still reads a
  version-1 file and appends to it as version 1, so an old session stays readable by the old
  binaries; `record_assembly` on it is `JournalError::AssemblyNeedsVersion2`.
- **How new p1 reads it.** `load`, `resume` and `open_for_append` accept 1 and 2; any other value
  is `JournalError::UnknownVersion` ("unknown journal version; refusing to guess"), never a
  guess.
- **Why and how an old binary refuses it.** The released binaries accept a header whose
  `p1_journal` is exactly 1 and refuse any other as an unknown version. A version-2 file
  therefore fails on its first line in an old binary, before any record is read: an old binary
  cannot check what executed the session, so it must not resume or replay it. The test
  `released_binaries_refuse_a_version_2_header` applies the released binaries' header check to a
  file `create` wrote; running the real old binary against a version-2 file is S1.9's
  (`journal_identity` in ADR-0080's Evidence).

## The assembly identity record (F3)

Version 2 adds one line kind beside the `seq` records: `{"assembly":{…}}`, owned by
`p1-journal` and not a `p1-contracts` record, so the core and its `CommitSink` are unchanged.

- **Payload.** `AssemblyIdentity { environment, host: HostIdentity { version, commit }, modules:
  Vec<ModuleIdentity> }`: the environment the session runs, the p1 binary that assembled it, and
  one entry per assembled module.
- **Per module.** `ModuleIdentity { name, kind, package, version, digest, abi }`:
  - `kind` is one of `tool`, `provider`, `context_policy`, `authorization_policy`;
  - `digest` is the SHA-256 hex of the package bytes the loader verified — the module's identity
    by [`package.md`](package.md#identity-the-digest) — and `null` for a native module, whose code
    the host's `commit` identifies instead;
  - `abi` is optional; `name`, `package` and `version` are strings.
- **Where it sits.** Between records. It carries no `seq`, does not count in the dense-seq rule
  and applies to the records that follow it until the next assembly line. `load` and `resume`
  report each as `AssemblyEntry { from_seq, identity }`, `from_seq` being the seq of the first
  record after the line. A torn assembly line is a truncated tail like a torn record.
- **Refusals.** Every type is `deny_unknown_fields`, so extending the identity is a format
  version bump, never a field an older reader drops. An assembly line in a version-1 file is
  `JournalError::AssemblyInVersion1 { line }`.
- **Durability.** `JsonlJournal::record_assembly` writes the line with the durability of a record
  commit under the store's `SyncPolicy`; `MemoryJournal::record_assembly` and `assemblies()`
  mirror it.
- **When it is written.** Per ADR-0080 the host writes one line before the first `Environment`
  record and again whenever the assembly changes (a model switch, a reconfiguration). That writer
  lands with S1.9, which builds the identity from the loader's `LoadedModule`.

The tests in `journal_version.rs` cover each rule: `create_writes_version_2`,
`version_1_file_loads_resumes_and_appends_as_version_1`, `version_3_is_unknown`,
`assembly_lines_round_trip_with_their_from_seq`, `assembly_line_in_a_version_1_file_is_refused`,
`assembly_line_with_an_unknown_field_is_refused`, `torn_assembly_line_is_a_truncated_tail_and_repairs`
and `released_binaries_refuse_a_version_2_header`.

## Gaps for S1

The boundary identifies a module by more than S1's record carries today. The fields below are
what the loader already knows about each module
([`loader.rs`](../../../crates/p1-module-runtime/src/loader.rs), `LoadedModule`, and the manifest
fields of [`package.md`](package.md#manifest-fields-frozen)); S1 decides how the record carries
them. None is invented here, and because every journal type denies unknown fields, each addition
is a format version bump by the record's own rule.

| Boundary field | Where the boundary has it | What S1's record has |
|---|---|---|
| Manifest `name` (`p1/<name>`, the reserved namespace) | manifest `name`, `LoadedModule::name` | `name` and `package` strings, with no rule yet for which one holds the manifest `name` |
| Digest | manifest `digest` as `sha256:<hex>`, `LoadedModule::digest` | `digest` as bare hex; the mapping between the two spellings is the writer's |
| World | manifest `world` (`p1:module/<kind>@1.0.0`) | not carried (`abi` is optional and undefined) |
| Protocol version | manifest `protocol`, checked by the loader against `PROTOCOL_VERSION` | not carried |
| Granted capabilities | manifest `capabilities`, `LoadedModule::capabilities`, what the host linked | not carried |
| Loader-built `ToolIdentity` (`implementation`, `variant`) | `LoadedModule::identity`, from manifest `name` and `variant` | `variant` not carried |
| Module class | the six manifest kinds, including `workflow-implementation` and `workflow-decision` | `ModuleKind` has four: `tool`, `provider`, `context_policy`, `authorization_policy` |
| The writer | — | lands with S1.9 (ADR-0080) |
