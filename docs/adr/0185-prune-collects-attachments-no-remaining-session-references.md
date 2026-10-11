---
adr: 185
title: Prune collects attachments no remaining session references
status: accepted
date: 2026-10-11
deciders: lead
supersedes: []
superseded_by: []
sources: ["https://github.com/5omeOtherGuy/phaseone/issues/803", "docs/adr/0180-attachments-are-references-in-history-a-new-user-content-item-and-a-content-addressed-local-store.md", "docs/adr/0171-interactive-sessions-are-journaled-by-default-in-one-state-root-session-catalog.md"]
---
# ADR-0185: Prune collects attachments no remaining session references

## Context

ADR-0180 keeps every attachment once in a content-addressed store under the state root,
"never replaced and never deleted". `p1 sessions prune` (ADR-0171) deletes session
directories but never the store, so attachment bytes grow on disk forever. Codex CLI and
OpenCode embed attachment bytes in the session record, Claude Code sweeps copies by age;
none shares one store between sessions. The owner ordered (#803, 2026-10-11) to keep the
shared store, which dedupes and keeps journals small, and tie each object's lifetime to the
sessions that reference it. This record narrows ADR-0180's "never deleted"; the rest of
ADR-0180 stands, so it does not supersede it.

## Decision

1. **Prune collects garbage.** After deleting the chosen sessions, `p1 sessions prune`
   deletes every object no remaining catalog session references; the dry run lists the
   objects and their bytes next to the sessions and deletes nothing. The collection covers
   the whole store whatever `--workspace` chose, and runs even when no session is old
   enough to go. With `--yes` the remaining sessions are listed again after the deletions,
   so a session whose deletion failed keeps its objects.
2. **References are addresses found in session files.** A session references every
   64-lowercase-hex run in any regular file of its directory (journal, worker journals,
   workflow runs), read as bytes and not parsed as records, so a replaced history, a
   truncated tail or a record kind added later still counts. Symlinks inside a session
   directory are not followed. Any read error, or a session directory or journal that is
   a symlink (p1 never writes one), stops the collection.
3. **Race rule.** A running session can save an object before its journal records it, so:
   (a) nothing is collected while any catalog session is live (its journal's writer lock
   is held, the catalog's `live`), and the scan probes each lock again and stops when one
   is held or cannot be probed; (b) an object last saved at or after the prune's own
   cutoff (`now - --older-than`) is never deleted; (c) a save that finds the object already
   stored sets its modification time to now, so a re-saved old object counts as young, and
   stores it again when collection removed it in between.
4. **Deletion safety.** Only `p1-attachment-local` deletes (`LocalAttachmentStore::collect`),
   through the pinned `objects` directory handle (`O_NOFOLLOW`, refused when shared): an
   entry is considered only when its name is a valid address and `statat` without following
   reports a regular file; `unlinkat` removes that entry, never a symlink's target and never
   anything outside the store. Staging files and other names are left alone. `collect`
   is an inherent method of the adapter, not part of the `AttachmentStore` port: retention
   is the host's maintenance of its own local store.
5. **A missing object never fails a request.** The host's provider decorator sends
   `[image unavailable: <id>]` for an image whose object is gone (the image route's other
   read failures keep ADR-0180's note), and `[file unavailable: <id> (<name>)]` instead of
   a path to a file object that is gone. A text-only route never reads the image and keeps
   its placeholder.

## Consequences

- Attachment bytes go when the last session naming them goes, after the same idle period.
- A journal outside the catalog (`--session FILE`) is invisible to prune: its attachments
  are collected once older than the cutoff, and a resume of it then sends placeholders.
- A live session anywhere postpones all collection until a prune when none is.
- A false keep is possible (an unrelated 64-hex string equal to an address); a false
  delete needs an object older than the cutoff that an unlocked session saves and records
  between the scan and the unlink, which (c) narrows to a re-save in that window.
- Prune reads every byte of every remaining session; it is a rare manual command.

## Alternatives considered

- Parsing records for `UserContent` references: misses worker journals and replaced
  histories unless every record kind is followed, and a new kind would silently free
  live objects.
- Embedding bytes in the journal (Codex, OpenCode): loses dedup and the small journal.
- Age-only sweeping (Claude Code): deletes objects a kept session still references.
- A reference-count file per object: a second truth beside the journals, and a crash
  between journal and count leaks or frees wrongly.
- A `collect` method on the `AttachmentStore` port: no other adapter or caller needs it yet.

## Evidence

`cargo test -p p1-attachment-local --test store` (`collection_deletes_old_unreferenced_objects_only`:
dry run, referenced, young, symlink and staging entries kept; `saving_an_object_again_makes_it_young`);
`cargo test -p p1-host --test session_history prune` (an object shared by a kept and a pruned
session survives, the pruned session's own goes, a young orphan survives, the dry run deletes
nothing, a live session stops the collection); `cargo test -p p1-host --lib attachments`
(`resume_with_collected_objects_sends_placeholders`: a journal loaded and projected after its
objects were deleted sends both placeholders).
