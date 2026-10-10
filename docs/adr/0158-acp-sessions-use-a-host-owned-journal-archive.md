---
adr: 158
title: ACP sessions use a host-owned journal archive
status: accepted
date: 2026-10-10
deciders: owner+lead
supersedes: []
superseded_by: []
sources: []
---
# ADR-0158: ACP sessions use a host-owned journal archive

## Context

Issue #691 of #670 requires journal-backed load, list, resume and fork at the dsh floor. ADR-0156 made ACP a router of isolated session processes but kept them in memory. #62's general CLI default session-location work remains open.

## Decision

ACP composes a `SessionArchive` port in p1-contracts and a host JSONL catalog adapter. It stores a private directory per opaque persistent id, with canonical workspace metadata and the existing journal format. The router passes `--session FILE` to each session child and `--resume` for load, resume and fork. Core, frozen TUI and ordinary CLI defaults are unchanged.

ASSUMPTION pending #62: the p1 state root is `P1_STATE_DIR`, else `$XDG_STATE_HOME/p1`, else `$HOME/.local/state/p1`; ACP uses `sessions/acp/<id>/journal.jsonl`. There is no retention or deletion policy in this slice.

## Consequences

Load and resume retain the id and obey existing journal locking, route/model checks and crash reconciliation. Load streams the recorded display history before its reply; resume streams none. An additive default front-end hook delivers the snapshot read under the writer lock. ACP does not learn host or core types. Model compaction does not erase the display transcript.

Fork creates a new id from a bounded validated append-only snapshot, preserving version, assembly identities, records and timestamps. It excludes a torn tail without modifying the source. A live parent's subsequent commits are independent. Running work, scratch files and output stores are not cloned. No recorded tool is re-executed. Only the host maps ids to paths. List filters canonical cwd and pages 100 entries at a time with an opaque cursor.

The stable wire names are `session/load`, `session/list` and `session/resume`; the v1 **unstable** fork shape still uses `session/fork` (not TypeScript's `unstable_forkSession` method name). Capabilities are advertised only when an archive is composed. This replaces ADR-0156's memory-only journal decision for ACP.

## Alternatives considered

One operator-supplied journal for the router cannot represent several sessions. Implementing #62's CLI picker/defaults here exceeds #691. A second transcript database would duplicate journal truth. Replaying only model history would hide compacted turns. Copying an unvalidated raw file could retain a torn tail; serializing the validated snapshot avoids it and retains assembly boundaries.

## Evidence

`cargo test -p p1-acp` covers router ids, list filtering, load/resume/fork and ordering. `cargo test -p p1-host acp` includes the recorded real-host journal load fixture and independent live-parent fork/resume test. The fixture continues with the old tool's file removed, proving display replay does not execute it. TCK and real client NOT RUN; the PR records verification results.
