# p1 roadmap (2026-10-10)

The roadmap is the GitHub epic **#763**, "Epic: p1 roadmap (2026-10-10, ADR-0153)". Its work
items are attached to it as sub-issues, and its body lists them in the current order of work. It
supersedes the epic #72 and the work list this page held from 2026-09-23 to 2026-10-10.

Three rules frame it:

- **The architecture is
  [ADR-0153](../adr/0153-p1-architecture-microkernel-ports-and-adapters-protected-variations-and-dsh-as-the-floor.md):**
  a small core with everything else in replaceable modules (microkernel), each module plugged
  into a declared interface (ports and adapters), an interface built as soon as a part is known to
  vary (Protected Variations), and dsh, the DeepSeek agent harness p1 is compared with, as the
  floor: no seam p1 builds may be less modular than dsh's. Before building a feature, read its row
  in the seam catalog [`seams.md`](seams.md) and build the port named there.
- **The gap list is the catalog's ranked table:** [`seams.md`](seams.md) "Feature gaps (not floor
  violations)". A gap gets its own issue when it is scheduled.
- **The order of work is the lead's** (ADR-0153 Decision 8). The epic records it; the lead
  changes it there, not on this page.

## History

Past record; neither part is a work list. The note and the table are kept as written in
September 2026; the "Outcome" paragraph and the closing line were added on 2026-10-10.

### Direction change (owner, 2026-09-25): WebAssembly modules

As written then: p1 migrates to WebAssembly modules (ADR-0071, issue #187): `p1-core`, the host,
the journal, the worker service and the terminal front ends stay native; every tool, provider and
policy becomes a module the host loads by name. No browser GUI for now. Builds for this migration
run on a dedicated AWS EC2 machine. The migration plan comes from Astra (brief and plan under
`../phaseone-briefs/astra/wasm-migration/`); its phases become issues under a new epic and take
precedence over [the 2026-09-23 order, since removed] where they overlap.

Outcome: the migration finished under the epic #206 (closed). The front ends went another way:
the terminal UI `p1-tui` is frozen as a parts donor (owner 2026-10-09) and `p1 acp` is the one
interactive door (ADR-0152, ADR-0154, ADR-0156).

### Session-log scan (2026-09-23): what the journals said

Seven DeepSeek workers scanned the 76 journals (99 findings, mapping in
`docs/research/session-log-scan-2026-09-23.md`). The recurring costs, in order of runs affected:

| what the journals show | issue |
|---|---|
| `finish` rejects `done` in most runs because the check ran in an `&&` chain; the suite is re-run | #73 |
| edits refused as unread or stale, even after the model's own edit; whole-file re-reads follow | #74 |
| argument bounds live only in the schema; `context` > 10 rejected 19 times across every model family | #75 |
| `read` refuses files outside the workspace that the brief names; the shell reads them | #76 |
| hours lost to shell timeouts on cargo and to two cargo calls deadlocking in one session | #77 |
| runs end on provider failures without a finish or a journal record; startup aborts leave nothing | #78 |
| summaries came back as raw markup or truncated; a resume re-summarized nothing into nothing | #79 |
| `cache_write` never reported on chat routes; uncached re-sends up to 1 134× without a compaction | #80 |
| git exits 128 inside sandboxed worktrees | #81 |
| shell summaries hide the failing lines; a worker's result arrives twice | #82 |

#73 and #81 are closed; where the others stand is in #763.
