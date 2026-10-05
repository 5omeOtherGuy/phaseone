---
adr: 111
title: Workspace mutations replace a leaf by exchange and refuse multiply linked files against an unsettled credential index
status: accepted
date: 2026-10-01
deciders: lead
supersedes: []
superseded_by: []
sources: ["https://github.com/5omeOtherGuy/phaseone/issues/468", "https://github.com/5omeOtherGuy/phaseone/issues/485"]
---
# ADR-0111: Workspace mutations replace a leaf by exchange and refuse multiply linked files against an unsettled credential index

## Context

After #467 a workspace mutation checks every target under the write gate, but writers
outside the gate (another process, or a sandboxed shell command running in parallel) still
had three windows (#468):

1. Between the final content check and the rename, an ungated writer could rewrite or
   replace the leaf, and the plain `rename` overwrote its change.
2. A symlinked `~/.config` (or XDG credential directory) re-pointed while a change was
   staged moved the protected directories, but the `CredentialPolicy` captured at the start
   of the apply was never derived again.
3. A parent directory created by the mutation (`Parent::missing`) was never re-proved: an
   actor that renamed it out of the workspace between staging and apply had the host write
   through the moved tree.

Separately (#485), the credential index's hard-link check differed site by site. The read
side and the read tool refused a multiply linked file unless the rebuilt index proved itself
current and settled; the search walk's opened-file check and the mutation's refreshed index
did not, so a candidate linked into `.config/keys` in the same coarse clock tick as the
index walk, or after the rebuild had enumerated that directory, passed.

## Decision

1. One rule in `p1-workspace`, `ProtectedIndex::refuses_unsettled_alias`: a multiply linked
   file is refused while the index it is checked against cannot prove itself current and
   settled (`still_current`). The module-runtime read side, the read tool's fallback, the
   search walk's opened-file check (an exclusion, as for a credential), search capability's
   windowed reads and the mutation's
   leaf checks all apply this one helper.
2. On Linux a write to an existing leaf exchanges the staged file with the leaf
   (`renameat2(RENAME_EXCHANGE)`) inside the pinned parent directory handle, then compares
   the swapped-out entry with the one checked under the gate: device, inode, size,
   modification time, link count and contents. On a mismatch the two are exchanged back and
   the change is refused with the existing "changed on disk" error. If exchange-back fails,
   the original entry is preserved at the temporary name and an explicit write-refused
   error names its current location for recovery. A new file is linked with
   `renameat2(RENAME_NOREPLACE)`, so a path filled after the checks is refused. Where
   `renameat2` answers `EINVAL`/`ENOSYS` (some FUSE filesystems) and on other platforms, the
   earlier path stays: the leaf's identity is checked once more and a plain rename follows;
   a create-only target and a rename destination still refuse there, as before.
3. The credential policy is derived again before each staged change and before the final
   checks, the index is rebuilt when it belongs to another policy, and the final checks test
   each leaf against the policy by its planned path and by the path its directory handle
   names now. Every step's directory is re-proved from the root through the names the plan
   walked and the parents staging created, before the first replacement; after a
   replacement or a rename the directory is proved once more and the step is undone when it
   moved. Undo of a newly created file supports the same plain-rename fallback, with a
   staged-identity and absent-destination recheck. If undo fails, an explicit write-refused
   error names the created file instead of implying nothing was written outside the root.

## Consequences

- An ungated writer can no longer lose a change to a write: a rewrite, a replacement or a
  new hard link that lands before the exchange is refused, and one that lands after it
  writes to a file that is no longer the target, as after any rename.
- The exchange sets the swapped inode's ctime on ext4 and tmpfs, so the post-exchange
  comparison leaves ctime out; the link count and the contents stand in for it. The final
  check before the exchange still compares ctime.
- A multiply linked file is refused (or excluded from a search) for as long as a protected
  directory changed within the index's coarse-clock margin (two seconds) of its walk. An
  ordinary hard-linked file in the workspace can therefore be refused briefly after a
  credential store changes; a retry after the margin succeeds.
- Removal is still an unlink after the final checks, so an ungated writer in that last
  interval can still lose a file it just replaced; the undo of a rename over an entry on a
  filesystem without `renameat2` cannot restore the replaced entry. Multi-file batches stay
  atomic per file only.

## Alternatives considered

- `O_TMPFILE` plus `linkat`: links a new file atomically but cannot replace an existing
  one, so it closes only the new-file case that `RENAME_NOREPLACE` already covers.
- Refusing whenever `renameat2` is unsupported: it would make every write fail on those
  filesystems; the earlier path with an identity re-check keeps them usable.
- Keeping the per-site hard-link checks: they had already diverged once (#485).

## Evidence

- `cargo test -p p1-workspace` — the regressions in `crates/p1-workspace/src/commit.rs`
  (`failed_exchange_back_names_and_preserves_the_original_entry`,
  `fallback_creation_is_undone_when_the_parent_moves_after_final_checks`,
  `failed_creation_undo_names_the_created_file`,
  `a_rewrite_after_the_final_checks_is_exchanged_back_and_refused`,
  `a_parent_moved_out_after_the_final_checks_keeps_nothing_written_outside`,
  `a_created_parent_moved_out_after_staging_is_refused`,
  `a_hard_link_into_the_protected_directory_after_the_final_checks_is_refused`,
  `a_credential_directory_repointed_while_staging_is_refused_at_apply`,
  `a_multiply_linked_target_is_refused_against_an_unsettled_index`,
  `without_renameat2_a_replacement_falls_back_to_the_identity_recheck`) and in
  `crates/p1-workspace/src/policy.rs`
  (`an_unsettled_index_refuses_a_multiply_linked_file_and_only_that`).
- `cargo test -p p1-module-runtime file_services` —
  `search_excludes_a_multiply_linked_file_while_the_index_is_unsettled` and
  `search_read_refuses_a_multiply_linked_file_while_the_index_is_unsettled`.
- The ctime behaviour of `RENAME_EXCHANGE` was observed on ext4 with a direct
  `renameat2` call (the swapped inode's ctime changed, its mtime did not).
