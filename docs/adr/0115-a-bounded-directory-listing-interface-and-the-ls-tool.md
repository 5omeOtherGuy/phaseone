---
adr: 115
title: A bounded directory-listing interface and the ls tool
status: proposed
date: 2026-10-05
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [ADR-0001, ADR-0087, ADR-0109, issue #512, audit slice E, DECISIONS D27 D30 D31]
---
# ADR-0115: A bounded directory-listing interface and the ls tool

## Context

Agents list directories through `shell ls` or the search tool's `list-files`. `list-files`
returns every file under a path, honours `.gitignore`, skips hidden entries and does not show
directories or symlinks, so it answers "which files" but not "what is in this directory". A
shell listing has no workspace confinement of its own beyond the sandbox, no bound on entries
and no honest partial counts.

Issue #512 (tools slice E of the iris-tools audit, `~/.agents/xo/dispatch/p1-iris-tools-audit/AUDIT.md`
sections 4–5) asks for an `ls` tool with the schema `ls {path?, limit?, depth?, long?,
ignore?}`, symlinks listed and never followed, a scan ceiling enforced before accumulation
(donor `iris-agent/src/tools/ls.rs:29`, `SCAN_BUDGET = 10_000`), and an opaque continuation so a
wide directory can be read in pages with no entry skipped or repeated. The owner ordered tools
E–J after the clean slate and #575 (D27, D30) and chose GPT-6.1 Sol high workers for them (D31).
The issue's own rule: the tool ships only if a measurement shows it costs fewer tokens than
`shell ls` and grep on ten listing tasks.

## Decision

- **A new interface, `directory-listing`**, in `modules/wit/workspace.wit`, allocated to the
  `tool` class only (`modules/capabilities.toml`) and granted only to `p1/ls`. It is additive
  in the way ADR-0109's `tool-outputs` was: the package stays `p1:module@1.0.0` and every
  component built against the earlier world still loads.

  ```wit
  interface directory-listing {
      use workspace.{fs-error};
      enum listed-kind { file, directory, symlink, other }
      record listed-entry {
          /// Relative to the workspace root, `/` separators.
          path: string,
          kind: listed-kind,
          /// Zero for anything but a file. Never the target's size for a symlink.
          size: u64,
          /// 1 for the listed directory's own entries, 2 for their children, ...
          depth: u32,
      }
      record listing-page {
          entries: list<listed-entry>,
          /// Present when the listing has more entries; passed back to continue.
          next: option<string>,
          /// Entries the host examined for this page, ignored ones included.
          scanned: u64,
          /// The page stopped at the scan ceiling, not at `limit` or the end: any total the
          /// tool reports is a lower bound.
          scan-capped: bool,
      }
      list-directory: func(path: string, depth: u32, limit: u32, ignore: list<string>,
                           continuation: option<string>) -> result<listing-page, fs-error>;
  }
  ```

- **The host owns the walk** (`p1-workspace`, through `p1-module-runtime`'s file services):
  confinement as for every workspace call, credential locations refused as for `stat` and
  `read`, symlinks reported as `symlink` and never followed or descended, depth-first in
  bytewise name order with directories and files interleaved by name, hidden entries included,
  `ignore` globs matched against the relative path. A page returns at most `limit` (1..500)
  entries and holds no more than that in memory: within each directory it selects the
  `limit` bytewise-smallest names after the continuation while reading the directory, rather
  than collecting and sorting the whole directory. Global bytewise order with a stateless
  continuation means every page reads every name of each directory it visits, so the work bound
  is a ceiling on names read per call, 100,000 (counted as names are read). A directory whose
  names would exceed it is refused with `io("<path> has more than 100,000 entries; list it with
  a glob")` and never listed partially or out of order; `scan-capped` marks a page that stopped
  at the ceiling between directories, and totals on it are lower bounds.
- **The continuation is the last returned entry's relative path**, encoded opaquely; the host
  resumes strictly after it in the same order and re-confines it like any path, so a forged
  continuation can only name a position inside the workspace. A directory changed between pages
  yields no repeated entry and skips only entries that were removed.
- **The `p1/ls` tool** (`p1-tool-ls` guest, a tool-class component) formats pages: one line per
  entry, indented by depth, `/` after directories, `@` after symlinks, sizes with `long`, then a
  footer with the counts and, when there is more, the continuation to pass back. Its schema is
  the issue's plus `cursor?: string` (the continuation; the issue requires paging but its schema
  listed no field for it). Path and glob validation are the host's; the guest never sees a path
  outside the workspace.
- **Shipping rule**: the implementing PR measures tokens (tool declaration + results +
  follow-up calls) for ten listing tasks with `ls` against `shell ls`/`list-files`. If `ls` is
  not smaller in total, the tool is not added to the shipped environments and the issue reports
  the numbers.

## Consequences

- A model can list a directory or a shallow tree in one bounded call, see symlinks as symlinks,
  and page through a 50,000-entry directory without the host holding it in memory.
- One more interface in the tool world and one more component; the allocation table and the
  boundary check change with it, and only `p1/ls` gains the capability.
- Paging is by position, not snapshot: a directory that changes between pages is listed as it is
  when each page is read.
- If the measurement fails, the interface stays (cheap, tested) and the tool is unshipped.

## Alternatives considered

- **Widen `workspace.list-files`** (directories, symlinks, hidden entries, paging): changes the
  meaning of a call every file tool already uses, and grants the listing to all of them.
- **A shell-only answer** (`shell ls`): no confinement of its own, no bound, no honest totals.
- **Snapshot paging** (host keeps a listing between calls): state per agent in the host, and a
  leak when the model never asks for the next page.

## Evidence

- Donor scan ceiling and listing behaviour: `~/projects/iris-agent/src/tools/ls.rs:3`, `:29`, `:31`.
  The donor collects and sorts a whole directory level before its 10,000-entry budget
  (`ls.rs:217-251`); a ceiling on examined entries cannot coexist with global bytewise order and
  a last-path continuation (the implementing worker's finding, 2026-10-05), so this ADR bounds
  memory by selection and work by names read per call instead.
- Additive boundary precedent: ADR-0109, `docs/design/modules/wit.md` "Amendments after the freeze".
- To re-check after implementation: the union test over a 50,000-entry directory (every entry
  exactly once across pages), memory measured during it, the symlink and confinement tests, and
  the token table in the implementing PR.
