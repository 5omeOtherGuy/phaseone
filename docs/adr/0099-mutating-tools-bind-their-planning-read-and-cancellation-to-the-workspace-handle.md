---
adr: 99
title: Mutating tools bind their planning read and cancellation to the workspace handle
status: accepted
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0099: Mutating tools bind their planning read and cancellation to the workspace handle

## Context

The native patch and edit tools locate their hunks by reading the target file before the
gated write. Two Codex findings on PR #467 showed that this read and the single write were
not bound to the object they were validated against:

- `crates/p1-tool-patch` refused credential paths in `Workspace::refuse_mutation_credentials`
  and only then called `Workspace::read_unobserved`. An ungated actor could swap the leaf for
  a symlink or hard link to a credential between the two calls, so planning materialized the
  credential bytes and a guessed hunk became a content oracle ("did not match" when wrong,
  the later commit refusal when correct).
- `crates/p1-tool-edit` committed with the model's spelling and content hash alone, so a
  symlink retargeted after the snapshot to another file holding the same, already observed
  bytes was overwritten instead of refused as stale.
- `crates/p1-tool-edit` used the component-facing `OwnedMutation::write`, which applies
  without a cancellation token, so a call cancelled while queued on the write gate could
  still mutate a file while staging.

## Decision

Add two methods and route the native tools through them:

- `Workspace::read_unobserved_checked`: like `read_unobserved`, but it refuses a credential
  by path, opens the leaf once, runs the credential identity check (`refuses_opened`,
  `ProtectedIndex::refuses_metadata`, `refuses_current_exact`) on that opened handle, and
  reads the bytes from the same handle.
- `OwnedMutation::write_cancellable`: like `write`, but it carries the `OwnedMutation`'s
  `ReadRecord` identity for the target and rechecks the caller's token after the gate is
  held and after staging, so a queued cancelled write mutates nothing.

The native patch plans through `read_unobserved_checked`; the native edit records the
snapshot's resolved path in a `ReadRecord`, acquires the gate with `begin_owned`, and applies
with `write_cancellable`.

## Consequences

A credential alias swapped in after the path check is refused even when the checked read
would otherwise return the credential bytes; the patch match can no longer confirm a guessed
credential hunk. An edit whose request spelling resolves to another file than the one it
snapshotted is refused as stale, matching the component's `ReadRecord` path check, even when
the bytes are equal. A cancelled queued edit writes nothing.

The checked read builds a `ProtectedIndex` per planning read, so a patch that reads many
files pays an index build per file; the native patch already built one per resolved path in
`refuse_mutation_credentials`, so the added cost is bounded to the planning reads. The
`OwnedMutation::write` and `Workspace::read_unobserved` behaviours are unchanged for the
component host calls.

## Alternatives considered

- Repeat `refuse_mutation_credentials` immediately before `read_unobserved`. Rejected: the
  check and the read still open different handles, so the swap window remains and the
  finding is not closed.
- Resolve the edit's path once and pass the resolved path to `commit_cancellable`. Rejected:
  it fixes only the edit's retarget case, drops the `ReadRecord` identity the component uses,
  and does not give the edit a cancellable single write.
- Change `Workspace::read` to check credentials for every caller. Rejected: every search read
  would then pay for, and be refused by, a credential walk it already filters, and the read
  tool's current messages would change.

## Evidence

- `crates/p1-workspace/src/read.rs`: `read_unobserved_checked` opens, checks and reads one handle.
- `crates/p1-workspace/src/commit.rs`: `OwnedMutation::write_cancellable`; the index build in
  `apply_with_hooks` and `refresh_credential_index` now take the caller's token.
- `crates/p1-tool-edit/src/lib.rs`: `a_symlink_retargeted_after_the_snapshot_is_refused`.
- `crates/p1-tool-patch/src/lib.rs`: `read` uses `read_unobserved_checked`.
