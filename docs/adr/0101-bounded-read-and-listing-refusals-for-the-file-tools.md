---
adr: 101
title: Bounded read and listing refusals for the file tools
status: accepted
date: 2026-09-29
deciders: lead
supersedes: []
superseded_by: []
sources: ["docs/design/tools.md"]
---
# ADR-0101: Bounded read and listing refusals for the file tools

## Context

The R3 fixes bound two host allocations that the file tools made from untrusted input,
but the bounds change public tool calls that used to succeed:

* The `read` component accumulated the whole file in the guest before rendering, and the
  host `workspace` capability buffered an opened file with no ceiling. A single `read` of
  a very large file could exhaust the guest's memory or the host's, and the snapshot was
  not checked after `stat`, so a file that grew between `stat` and the first window was
  read past every earlier size check.
* `workspace.list-files` and the file listing inside a `mode:"files"` search retained one
  path per file with no limit. A workspace with millions of files could make the host hold
  an unbounded path vector and the guest an unbounded `list<string>`.

Both are interface-level behaviour changes: a call that once returned content (or a path
list) now returns an error. `AGENTS.md` requires an ADR before such a change lands, and
the design docs already describe the resulting limits (`docs/design/tools.md`).

## Decision

The file tools refuse input that exceeds fixed, documented budgets instead of allocating
unboundedly, and every refusal is an explicit model-facing error.

1. A `read` of a file whose `stat` size exceeds 8 MiB is refused before the guest allocates
   a copy (`file exceeds the component read budget`). The host side of a component read
   opens the checked descriptor, holds at most 8 MiB plus one detection byte in the
   snapshot, and refuses growth past that budget even when `stat` understated the file.
2. A `workspace.list-files` walk (including the listing behind an empty-pattern
   `mode:"files"` search) refuses once it would retain more than 4,096 paths or more than
   512 KiB of displayed and absolute path bytes, with `workspace listing exceeds the
   bounded search budget; narrow with path or glob`. Content searches stream the walk in
   bounded chunks (at most the same 4,096-path / 512 KiB buffer per chunk, each chunk
   sorted by displayed path and merged into one bytewise-ordered result) and keep only
   bounded match lines, so a single very wide directory is never collected whole, the
   result keeps the listing's bytewise order across chunks, and the search is not subject
   to the listing refusal.
3. A patterned `mode:"files"` search that needs a top-up listing treats that same refusal
   as "no more paths are available": it renders the paths its first bounded search already
   carried, with the exact omitted count, instead of failing the call.
4. The budgets are constants in the runtime (`p1_module_runtime::file_walk` and
   `p1_module_runtime::file_services`) and in the read module (`MAX_GUEST_READ_BYTES`);
   they are not configuration. A future paginated `list-files` capability removes limit
   (2) and the streaming-observation capability removes limit (1).

## Consequences

* A large-file `read` and a very large directory listing now fail with a message that says
  what to narrow or how to proceed, rather than succeeding and risking memory exhaustion.
* The limits are conservative: a file between 8 MiB and the old unbounded size is no longer
  readable through the component until streaming observation exists, and a workspace above
  4,096 files cannot be listed until pagination exists. The `mode:"files"` top-up keeps its
  bounded answer inside that limit.
* The refusals are part of the frozen `fs-error` `io` text and are asserted by the tools'
  own tests, so changing a budget is an interface change that needs a new ADR.

## Alternatives considered

* **Let the host hold the whole file/listing and rely on the model to narrow.** Rejected:
  the input is untrusted and the allocation is host memory, not the model's choice.
* **Truncate the read or the listing silently.** Rejected: a partial read would be
  presented as complete, and a partial listing would misreport which files exist. An
  explicit refusal is honest.
* **Paginate the `list-files` capability now.** Deferred: it changes the WIT capability,
  the generated bindings and every caller; the bounded refusal is the smaller safe change
  for this round.

## Evidence

* `crates/p1-module-runtime/src/file_walk.rs`: `MAX_WALK_FILES`, `MAX_WALK_PATH_BYTES`,
  `collect_files`, `search_streaming`/`search_chunk`; tests
  `listing_refuses_before_collecting_unbounded_paths`,
  `broad_content_search_streams_past_listing_budget`,
  `a_single_wide_directory_is_searched_in_bounded_chunks`,
  `chunked_search_keeps_one_global_bytewise_order` and
  `streaming_search_orders_names_by_displayed_path`.
* `crates/p1-module-runtime/src/file_services.rs`: `MAX_COMPONENT_READ_BYTES`,
  `snapshot_from_open_file`; test `bounded_host_snapshot_refuses_growth_after_stat`.
* `modules/p1-module-read/src/lib.rs`: `MAX_GUEST_READ_BYTES`; tests
  `oversized_guest_chunk_refuses_without_observation` and
  `refuses_oversized_stat_before_guest_buffering`.
* `crates/p1-tool-search/logic/src/exec.rs`: `matching_files`,
  `is_bounded_listing_refusal`; test
  `files_mode_keeps_the_bounded_result_when_the_top_up_listing_is_too_large`.
* Limits described in `docs/design/tools.md` under `read` and `grep`.
