# Journal: the version record and the assembly identity

Status: published freeze item 7 of the WebAssembly boundary (ADR-0071, review finding F3): a
version-bearing record that old binaries fail on, and the assembly identity record. The journal
is native and stays native ([ADR-0081](../../adr/0081-native-foundation-and-runtime-components.md));
its code is S1's. This document publishes what that code does today — journal format version 2
with assembly identity lines, landed by S1.3 (PR #228, merge commit `6d6978f1`) and written by the
host since S1.9 under [ADR-0080](../../adr/0080-execution-manifests-in-journals.md) — and the field
mapping S1.9 fixed under "What S1.9 decided". The store's full specification is
[`docs/design/journal.md`](../journal.md); the code is
[`crates/p1-journal/src/lib.rs`](../../../crates/p1-journal/src/lib.rs), its tests are
[`crates/p1-journal/tests/journal_version.rs`](../../../crates/p1-journal/tests/journal_version.rs),
the writer and the changed-artifact report are
[`crates/p1-host/src/run.rs`](../../../crates/p1-host/src/run.rs) and its tests are
[`crates/p1-module-tests/tests/journal_identity.rs`](../../../crates/p1-module-tests/tests/journal_identity.rs).

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
  file `create` wrote, and S1.9's
  [`journal_identity.rs`](../../../crates/p1-module-tests/tests/journal_identity.rs) applies it to
  the version-2 file it writes for the pinned release binary to refuse (ADR-0080's Evidence).

## The assembly identity record (F3)

Version 2 adds one line kind beside the `seq` records: `{"assembly":{…}}`, owned by
`p1-journal` and not a `p1-contracts` record, so the core and its `CommitSink` are unchanged.

- **Payload.** `AssemblyIdentity { environment, host: HostIdentity { version, commit }, modules:
  Vec<ModuleIdentity> }`: the environment the session runs, the p1 binary that assembled it, and
  one entry per assembled module.
- **Per module.** `ModuleIdentity { name, kind, package, version, digest, abi }`:
  - `kind` is one of `tool`, `provider`, `context_policy`, `authorization_policy` — the four
    classes an agent is assembled from. The six manifest kinds include the two `workflow-*` ones,
    which are never assembled into an agent, so the record needs no more;
  - `name` is the module's own identity: the manifest `name` (`p1/<name>`) of a package, or, for
    a native module, the crate that provides it (`p1-tool-read`) or the manifest name of the
    component that will replace it (`p1/context/summarizing`, `p1/policy/ask`);
  - `package` is the key the environment selects the module by: the `modules.lock` key of a
    package (the `[[tools]] module = …` key, or the provider key for a provider package), or the
    catalog key of a native module (`read`; for the two host policies the host's own selection,
    which is the policy's manifest name);
  - `version` is the release version the lock pins for a package, and the p1 version for a native
    module (which is the same binary);
  - `digest` is the SHA-256 of the package bytes the loader verified, in the BARE lowercase hex
    spelling ([`package.md`](package.md#identity-the-digest) writes it `sha256:<hex>`; the mapping
    between the two spellings is the writer's), and `null` for a native module, whose code the
    host's `commit` identifies instead;
  - `abi` is the package's world and the value protocol it speaks, `<world>+<protocol>` — e.g.
    `p1:module/tool@1.0.0+1.0` — and `null` for a native module, which has no WIT world.
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
- **When it is written (S1.9).** The host builds the identity in `p1_host::run` — from the
  assembled tools, the environment's provider key, the two host policies and the `modules.lock`
  the catalog's own module registration read — and writes it through the sink the core commits
  through, immediately before the run's first record: the `Environment` record is first in a
  turn, so the line precedes it, and a resume whose first request the provider refuses commits
  nothing and so writes nothing. A line is written again after every change that commits a new
  `Environment` (`/model` or `/effort` through `ModelSwitch`, and `Agent::reconfigure`), and on
  a resume that names another assembly; a resume over an unchanged assembly writes nothing and
  reports nothing. The lock's digest is the loader-verified one: the lock is checked against the
  release manifest and the loader verifies the compiled bytes against that same manifest digest.
- **The changed-artifact report (S1.9).** On resume the host compares the journal's last identity
  with the identity it assembled now and prints one line per module whose digest, package or
  version changed, plus every module added or removed, and the host and the environment when they
  differ. A changed artifact never blocks the resume: the journal's claim is reported, never taken
  silently. A version-1 file carries no line, so there is nothing to compare and nothing to say.
- **A version-2 file that names no assembly** (a file whose first line was written and whose
  writer stopped before the line) gets the identity of the assembly running now, and no report:
  there is no claim to compare against.

The tests in `journal_version.rs` cover each rule: `create_writes_version_2`,
`version_1_file_loads_resumes_and_appends_as_version_1`, `version_3_is_unknown`,
`assembly_lines_round_trip_with_their_from_seq`, `assembly_line_in_a_version_1_file_is_refused`,
`assembly_line_with_an_unknown_field_is_refused`, `torn_assembly_line_is_a_truncated_tail_and_repairs`
and `released_binaries_refuse_a_version_2_header`.

## What S1.9 decided

The boundary identifies a module by more than S1.3's record carried on its own. The fields below
are what the loader already knows about each module
([`loader.rs`](../../../crates/p1-module-runtime/src/loader.rs), `LoadedModule`, and the manifest
fields of [`package.md`](package.md#manifest-fields-frozen)); S1.9 fixed how the record carries
them, without a format version bump: every value fits the six fields the record already has, so
none of the mappings below adds a field. ADR-0080 records the decision.

| Boundary field | Where the boundary has it | How the record carries it |
|---|---|---|
| Manifest `name` (`p1/<name>`, the reserved namespace) | manifest `name`, `LoadedModule::name` | `name` holds it; `package` holds the lock key the environment selects the module by |
| Digest | manifest `digest` as `sha256:<hex>`, `LoadedModule::digest` | `digest` as bare lowercase hex; the writer strips the `sha256:` prefix |
| World | manifest `world` (`p1:module/<kind>@1.0.0`) | `abi` = `<world>+<protocol>` |
| Protocol version | manifest `protocol`, checked by the loader against `PROTOCOL_VERSION` | the second half of `abi` |
| Granted capabilities | manifest `capabilities`, `LoadedModule::capabilities`, what the host linked | not carried: the digest pins the manifest that grants them, so the same bytes cannot gain a grant |
| Loader-built `ToolIdentity` (`implementation`, `variant`) | `LoadedModule::identity`, from manifest `name` and `variant` | not carried: the manifest `name` is one variant's package, and a call's identity is in the record it belongs to |
| Module class | the six manifest kinds, including `workflow-implementation` and `workflow-decision` | `kind` carries the four classes an agent is assembled from; a `workflow-*` package is never assembled into an agent |
| The writer | — | `p1_host::run` (S1.9): the section "When it is written (S1.9)" above |

The record needs no format version bump for any of them, so the format stays version 2.
