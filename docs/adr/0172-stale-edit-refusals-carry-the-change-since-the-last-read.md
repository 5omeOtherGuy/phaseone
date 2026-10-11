---
adr: 172
title: Stale edit refusals carry the change since the last read
status: accepted
date: 2026-10-11
deciders: lead
supersedes: []
superseded_by: []
sources: [ADR-0025, ADR-0071, ADR-0088, ADR-0109, ADR-0115, issue #74, issue #766]
---
# ADR-0172: Stale edit refusals carry the change since the last read

## Context

Read-before-mutate (ADR-0025) refuses an edit of a file that changed on disk since the agent
last observed it with "X changed on disk since you last read it; read it again." The
session-log scan behind issue #74 counted stale refusals after shell and heredoc edits, each
followed by a whole-file re-read (2 000 lines by default). The owner trimmed #74 on 2026-10-11
to this: the stale refusal carries a unified diff of the change since the last read, capped at
200 lines, so the model can re-issue the edit without re-reading the file.

The observed-file registry (`p1-workspace`'s `ObservedFiles`) kept only a content digest, and
the `snapshot` interface of `modules/wit/workspace.wit` answers `check` with an enum, so neither
the native tool nor the `edit` component could know what the file held when it was observed.

## Decision

- `ObservedFiles` keeps the bytes of each observation of a file up to 512 KiB beside its
  digest, within 16 MiB per agent store, evicting the oldest retained bytes first and never a
  digest. A streamed observation keeps its digest only. Staleness is still decided by the
  digest alone.
- The `snapshot` interface gains one function, additive in the way ADR-0109 and ADR-0115 were
  (the package stays `p1:module@1.0.0`; a component built against the earlier interface still
  links):

  ```wit
  observed: func(path: string) -> result<option<list<u8>>, fs-error>;
  ```

  `none` when the path was never observed or only its digest is kept. The host resolves the
  path under confinement as for `check`. `SnapshotService::observed` defaults to `None`.
- The refusal text and the diff live in `p1-tool-edit-logic` (`stale.rs`), which the native
  tool and the component share: the first line stays "X changed on disk since you last read
  it.", followed by a unified diff of the text an edit matches (BOM stripped, line endings
  normalized), three lines of context, at most 200 diff lines with a note naming how many more
  there are, lines over 500 bytes cut. Without kept bytes, or when either side is not UTF-8 or
  only the BOM or line endings changed, the refusal is the earlier plain text.
- A refusal that shows the whole change (no cap, no cut line) records the current contents as
  the agent's observation, so the edit re-issued from the diff is accepted without a read. A
  capped or plain refusal records nothing.

## Consequences

- A stale edit costs one refusal with the changed lines instead of a refusal plus a whole-file
  read. The write gate's recheck is unchanged: a change between the refusal and the re-issued
  edit is refused again.
- Each agent's registry holds up to 16 MiB of file bytes it already read.
- Only `edit` uses the new function; `write` and `patch` keep their refusals. The host's
  refusal under the gate (a race inside one call) stays plain.

## Alternatives considered

- Return the diff from the host (a richer `check` result or an `io` message): the model-facing
  text would live in host code rather than the tool's shared logic, and changing `check`'s
  result type would break every built component.
- Keep the old contents in the edit component: a component instance does not outlive a call
  and does not see the read tool's observations.
- Record the observation on every refusal, capped or not: the model would be allowed to edit a
  file whose change it saw only in part.

## Evidence

`cargo test -p p1-tool-edit-logic` (`stale::tests`, `exec::tests::a_target_changed_since_it_was_read_is_refused`,
`exec::tests::the_edit_reissued_from_the_refusals_diff_lands_without_a_read`,
`exec::tests::a_capped_refusal_still_requires_a_read`), `cargo test -p p1-tool-edit`
(`guest_parity::the_stale_refusals_diff_matches_the_native_tool`) and `cargo test -p p1-workspace observe`.
