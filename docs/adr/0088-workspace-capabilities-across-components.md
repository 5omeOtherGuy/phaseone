---
adr: 88
title: Workspace capabilities across components
status: proposed
date: 2026-09-26
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [S2 brief .wasm/down/BRIEF.md (stream S2 filesystem tools; rows U-adr and U-cat), S2 BLOCKERS.md S2-B1 S2-B2 S2-B3 S2-B4, docs/adr/0025-workspace-confinement-and-read-before-mutate.md, docs/adr/0032-agents-sharing-a-directory-serialize-their-file-mutations.md, docs/adr/0057-a-tool-describes-each-call-s-target-the-host-and-the-ui-stop-matching-tool-names.md, docs/adr/0059-a-tool-describes-its-results-and-its-destructiveness-the-host-describer-keeps-no-tool-name-table.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0081-native-foundation-and-runtime-components.md, docs/adr/0082-component-abi-and-execution-ownership.md, docs/design/modules/wit.md, docs/design/modules/protocol.md, docs/design/modules/package.md, docs/design/modules/workspace-mutation.md, modules/wit/workspace.wit, crates/p1-workspace/src/]
---
# ADR-0088: Workspace capabilities across components

## Context

ADR-0071 moves every tool into a WebAssembly component, and ADR-0081 keeps workspace
confinement and atomic writes native. The boundary frozen at `wasm-boundary-v1` publishes the
three workspace interfaces in `modules/wit/workspace.wit` — `workspace` (`stat`, windowed
`read`, `list-files`, `search`), `snapshot` (`observe`, `check`) and `workspace-mutation`
(`begin` and the `mutation` resource with `write`, `create`, `remove`, `rename`) — and names
`p1-workspace` as their owner (`docs/design/modules/wit.md`, freeze item 3, F9). It does not
say how the file tools' invariants survive the move.

Those invariants are ADR-0025 and ADR-0032. ADR-0025: every path resolves inside the root after
symlinks, always; `edit` and `write` refuse a file the agent never observed or that changed
since; staleness is decided by content; `apply_patch` is exempt from read-before-mutate because
its hunks carry their own staleness check, and it still records what it wrote. ADR-0032: agents
sharing a directory share one `WriteGate`, observation registries stay per agent, and a
mutation holds the gate from reading the current contents to recording the write. Natively each
tool does this on a blocking thread with direct access to a path, the gate and the agent's
`ObservedFiles`. A component has none of these: no WASI (D-XO-4), no thread, no path, only the
imports its assembly links, and a `mutation` resource that does not outlive the export call
that began it (`wit.md`, "Streaming resources").

Stream S2's brief (U-mut) asks for a `commit` that validates every target, takes the gate,
rechecks snapshot identity and observations, stages, writes atomically per file and releases
the gate; the frozen WIT has no `commit` function (S2 BLOCKERS.md S2-B4). The brief's U-cat row
fixes the capability allocation of the filesystem tools. ADR-0057 is also touched: a component
describes its calls through the JSON call-description family, whose verb the boundary maps onto
a closed vocabulary instead of changing `CallDescription.verb` (freeze item 8, the F4
decision in `docs/design/modules/protocol.md`).

How the host implements the three interfaces is proposed in
`docs/design/modules/workspace-mutation.md`, which this ADR cites.

## Decision

1. **ADR-0025 placement.** `workspace`, `snapshot` and `workspace-mutation` are implemented by
   the native `p1-workspace` service (F9); the host functions only map WIT arguments onto it
   and `WorkspaceError` onto the frozen `fs-error`. Confinement after symlink resolution,
   read-before-mutate and content-based staleness stay host-enforced on every call. Guest
   preopens stay empty: a component never holds a path or a descriptor, and every path it
   names is resolved again by the host.

2. **ADR-0032 preserved.** Agents sharing a directory share one `WriteGate`; each agent keeps
   its own observations, a parent and its workers separately. A mutation holds the gate from
   the host's recheck to the recorded write, and a mutation does not outlive the export call
   that began it: the host releases the gate when the guest drops the resource or when that
   call returns, and a later use traps. A trap never undoes a change already applied.

3. **`commit` is the native API behind the frozen `mutation` resource** (S2-B4 option a, no
   WIT change). A component validates, reads and computes outside the gate, then `begin`s a
   mutation; each `write`, `create`, `remove` and `rename` is one `commit` of one change, which
   under the gate rechecks the target — its contents against what this call read, and in
   observed mode the agent's observation — then writes atomically: `write` and `create`
   stage the contents in a sibling temporary file and rename it over the target, `remove`
   unlinks the target, and `rename` moves the source. The written contents are recorded as
   the agent's observation; a removed or renamed-away path is forgotten. Atomicity is per
   file only:
   a multi-file change is serialized against other file tools but has no crash atomicity
   across files. The recheck, the staging and the write, unlink or move are directory-relative
   operations on directory handles opened without following symlinks, which closes the symlink
   replacement races between resolution and write.

4. **The patch exemption is a per-assembly grant, not a WIT capability.** The host catalog
   links `workspace-mutation` in one of two modes: *observed* (read-before-mutate enforced) or
   *patch-authorized* (the observation check skipped, the call's read-identity check and the
   recording kept). A component cannot ask for the exemption; it can only be assembled with it.
   Search reads without recording an observation and so can never obtain edit permission. The
   allocation per tool is the brief's U-cat row plus the one grant `edit` and `write` need to
   keep the native order of refusals: read — workspace metadata and read with observations;
   search — metadata, list and read without observations; edit and write — workspace read plus
   observed mutation, and `snapshot` (`check` and `observe`); patch — workspace read plus
   patch-authorized mutation. The native edit reports "You must read … before changing it."
   before it matches `old_string`, and the native write checks an existing target before it
   writes; a component that computed first and learned the observation state only from its
   mutation would show a different error in a different order. `snapshot.check` is what tells
   the component the state outside the gate, and `snapshot.observe` records the contents a
   change wrote, as the native tools record them. The host's recheck under the gate is
   unchanged by the grant and stays the enforcement.

5. **ADR-0057 per the F4 decision.** Call descriptions keep ADR-0057's public shape unchanged:
   `CallDescription.verb` stays a `&'static str`, a component's verb crosses as a string and is
   mapped by `call_verb` onto the closed vocabulary of `protocol.md` (`read`, `edit`, `run`,
   `search`, `finish`, `worker`, `workflow`, `call`), and anything else becomes `call`. No
   public type changes. The four filesystem tools use verbs of that vocabulary today: `edit`
   for `edit`, `write` and `apply_patch`, and `search` for `search`, so their components keep
   their verbs exactly.

## Consequences

- The file tools' invariants hold whatever a component does: an escaping path, an unread or
  stale target and an unserialized write are refused by native code the guest cannot reach.
- The gate is held for a recheck and a few file operations only, never while a component
  computes; no `spawn_blocking` sits on a guest path, because `begin` is an asynchronous host
  import that suspends the guest.
- Computing outside the gate narrows one native behaviour: a patch whose file another agent
  wrote between the patch's read and its mutation is refused as stale and applied again by the
  model, where the native tool re-matched its hunks under the gate. No update is lost either
  way. A component that needs the re-match may `begin` before it reads, as the WIT allows.
- A recheck refusal reaches the component as `fs-error.io` with the host's message (the native
  stale-file texts), because the frozen `fs-error` has no staleness case; a new case would be a
  freeze amendment.
- The host's mapping onto `fs-error` is fixed so that a component's text can be the native
  tool's byte for byte: `wrong-kind` is a path that exists but is not the kind the call needs,
  and `io(message)` carries what the native tool prints for the failure — the `std::io::Error`'s
  own text, never a host path. `wrong-kind` covers two native errors (a directory read is
  `Is a directory`, a file used as a directory component, such as `d.txt/x`, is `Not a
  directory`), so a component decides its text from the kind it stat'd — and, when the stat
  itself failed, from that — and both come out as the native tool words them
  (`docs/design/modules/workspace-mutation.md`).
- The patch exemption lives in S2's catalog rows, so it is visible where a tool is assembled
  and cannot be claimed by a package manifest.
- Multi-file crash atomicity remains absent, as it is for the native `apply_patch`; a patch
  still validates every hunk before its first change.
- Until the loader links the workspace interfaces (S2-B3), components that import them load
  only once that linking exists; the guest logic is tested natively meanwhile.
- `scripts/gate.sh` (G4) cannot pass on the S2 box until `wasm-tools` 1.259.0 is installed
  there (S2-B1): implementation and review proceed, the G4 row and landing wait.

## Alternatives considered

- **A WIT batch `commit` export** that takes every change of a call at once: rejected for now.
  It would be a freeze amendment (S1 lead approval) for a guarantee the resource already gives
  — serialization and per-file atomicity — and it would not add multi-file crash atomicity by
  itself.
- **Guest preopens** (a WASI directory handed to the component): rejected. The guest target has
  no WASI (D-XO-4), and a descriptor in the guest would move confinement and the gate out of
  the host, where the guest could bypass them.
- **A separate WIT capability for patch** (a second mutation interface without the observation
  check): rejected; it widens the frozen WIT and the class allocation for what is an
  assembly-level decision.
- **Holding the gate across a component's reads and computation by rule**: rejected as the
  default; it serializes every file tool's computation across agents. It stays available to a
  component that begins its mutation first.
- **Changing `CallDescription.verb` to an owned `String`**: rejected by F4; it is a public type
  change for no gain to these tools, whose verbs are in the vocabulary.

## Evidence

None yet: this ADR merges `proposed` before the first S2 component lands. Its
evidence arrives with the P2 definition-of-done rows of the S2 brief — the native tool suites,
`cargo test --locked -p p1-module-tests --test filesystem_tools`, `--test filesystem_boundary`,
`--test descriptions`, `cargo test --locked -p p1-tool-tests --test shared_workspace`
(parent and worker observations stay separate), `cargo test --locked -p p1-workspace` and the
G4 gate row — and it is accepted with that Evidence in the PR that lands P2's last DoD row.
For decision 5, the audit of `p1-contracts` consumers covers every consumer pinned on a
public type S0's F4 decision changed: F4 changed no public type, so the consumer list is
empty. The audit line with its build and test output (`cargo test --locked -p p1-contracts
-p p1-module-protocol`) will be recorded in the PR that proposes this ADR.
