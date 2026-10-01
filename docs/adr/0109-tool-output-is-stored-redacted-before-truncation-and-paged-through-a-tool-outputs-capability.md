---
adr: 109
title: Tool output is stored redacted before truncation and paged through a tool-outputs capability
status: accepted
date: 2026-09-30
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/design/modules/wit.md, docs/design/modules/capabilities.md, docs/design/tools.md, docs/adr/0068-tool-output-is-masked-for-credential-shapes-before-history-journal-and-summaries.md, docs/adr/0071-p1-migrates-to-webassembly-modules-native-core-and-host-load-tools-providers-and-policies-by-name.md, docs/adr/0092-call-scoped-capability-services-and-a-hostcall-budget-sized-for-whole-files.md, docs/adr/0101-bounded-read-and-listing-refusals-for-the-file-tools.md, docs/adr/0108-credentials-are-registered-values-masked-everywhere-and-credential-files-are-written-through-a-pinned-directory.md]
---
# ADR-0109: Tool output is stored redacted before truncation and paged through a tool-outputs capability

## Context

A shell result reaches the model after two cuts: the host keeps at most 25,000 bytes / 990
lines of head and of tail and drops the middle (`crates/p1-module-runtime/src/process/mod.rs`,
`HEAD_BYTES`/`TAIL_BYTES`), and the guest squeezes the rendered result to 50,000 bytes
(`crates/p1-tool-shell/guest/src/lib.rs`, `squeeze`). The dropped bytes are never held and
cannot be recovered; `raw: true` does not bring them back (`docs/design/tools.md`, shell
filter contract). The declarative filters landed in #496 shrink output further. When the cut
removes the diagnostic that matters, the model's only option is to rerun the command.

iris and RTK keep what they cut: RTK tees the raw output to a file before filtering, iris
stores results over 50 KiB behind an `outputHandle` and pages them with `read_output`. The
audit of the iris tool set (issues #508-#517, audit in
`~/.agents/xo/dispatch/p1-iris-tools-audit/AUDIT.md`, sections 2.5, 4.2, 5.C-D) ranks raw
recovery as the first technique to take. The owner chose, 2026-09-30, in the question dialog:
slices A-D "as per your recommendation", i.e. the output store (C, #510) and `read_output`
(D, #511) in this round.

The `tool` world has had no new capability since the freeze (`wasm-boundary-v1`; v1.1 only
added two functions to `workers-observe`). A new interface is a boundary change
(`docs/design/modules/README.md`) and needs this record.

## Decision

1. **Capture point.** The host tees every shell process's interleaved output stream in
   `ProcessStream` before `Capture::push`, so the store receives each chunk exactly once,
   before the head/tail cut, the filters and the guest squeeze. Memory stays bounded by the
   chunk size; the store writes to disk.
2. **Redaction before persistence.** Each chunk passes through `p1_redact::redact_with` with
   the agent's `SecretSet` (`ToolServices.mask.secrets()`) before it is written, holding back
   the `held_suffix_len` tail across chunk boundaries as `crates/p1-host/src/secret_mask.rs`
   does, so a secret split over two chunks is still masked. Unredacted bytes never reach disk.
   Pages are read from the redacted file; the existing `RedactingTool` wrapper still applies
   to `read_output` results (ADR-0068, ADR-0108). The held text is bounded (64 KiB plus one
   chunk). A line longer than that with no safe cut is cut only where masking both sides apart
   equals masking them together; failing that, the run of credential-shaped characters that
   the cut crosses is masked on both sides until it ends (`<redacted:cut:N chars>`), so no
   piece of a credential is stored. An unbroken such run longer than about 48 KiB is stored
   masked, not verbatim. A cut is never placed where masking the two sides apart differs from
   masking them together for a registered secret (so a multi-line secret such as a PEM key is
   never split), and once a credential context has opened (a JSON auth key, `Authorization`,
   `Bearer`) everything up to its terminator is masked across any cut.
3. **Location and lifetime.** With `--session FILE` each run stores into its own random
   directory `FILE.outputs/run-<hex>/` (mode 0700, files 0600), beside the session file and its
   `FILE.w<N>.jsonl` workers, removed with the session. Without `--session` the store is a
   private temporary directory owned by the run and removed at exit. In both cases a handle is
   served only by the run that produced it, so the run removes its directory when it ends, and
   `FILE.outputs/` when that leaves it empty (#523); only a directory a killed run left behind
   stays on disk after `--resume`, counted against the session cap and never served.
4. **Handles.** A handle is an opaque host-scoped string id (as worker ids are, `wit.md`
   S0-R1.2), random, never a path. A handle from another session, a malformed handle or a
   removed output is `unknown-output`; no guest input selects a file. The store serves only
   files it wrote in the current run, checked against what it recorded when writing them; a
   file placed in the directory by anyone else is not served. (A signed on-disk index was
   rejected: any key p1 keeps on disk is readable by a shell running as the same user.)
5. **Interface.** A new interface `tool-outputs` in `modules/wit/outputs.wit`, package
   `p1:module@1.0.0` unchanged, imported by `world tool`, allocated to the `tool` class only
   in `modules/capabilities.toml`. It is additive: components built against the old world
   still load. Functions:
   - `produced() -> list<output-info>`: outputs stored during the current tool call
     (call-scoped service, ADR-0092), so the shell guest can name the handle in its result
     without any change to `process`.
   - `describe(handle) -> result<output-info, output-error>`.
   - `page(handle, offset: u64, limit: u32) -> result<output-page, output-error>`: a
     zero-based UTF-8 byte cursor; a page never splits a character; a limit too small for the
     next character is `limit-too-small`, never an empty page with the same cursor.
   - `output-info` carries the handle, stored bytes and a `capture` state: `complete`,
     `stored-cap-reached` (the store stopped writing at its per-output cap; what was stored
     is exact), `storage-incomplete` (the write queue filled, so storing stopped; the file
     holds an exact prefix), or `storage-failed` (nothing recoverable, including a writer that
     has not finished 2 s after the call; no handle is offered to the model, although
     `produced()` lists it and its handle resolves to `unknown-output`).
   - Storing never slows or fails the command: writes run off the draining path through a
     bounded queue.
   - Errors: `unknown-output`, `limit-too-small`, `offset-past-end`,
     `offset-inside-character`, `read-failed`. A page is at most 1 MiB whatever `limit` asks.
   Producing outputs is host-only; no guest can write to the store.
6. **Grants.** The shell package gains `tool-outputs`; the new `read_output` tool (#511) is
   the only other holder. Every other package is refused the import (`UndeclaredImport`).
7. **Disk bound.** Each output has a byte cap and the store has a per-session cap, both host
   configuration, reported in `output-info` when reached. Defaults (`OutputCaps::DEFAULT`):
   16 MiB per output, from the #510 measurement (largest single output of the measured set
   12,711,230 bytes, `git log -p -n 200`; one run, raw bytes before masking); 256 MiB per
   session, kept by #523: 13 recorded p1 coding sessions (2026-09-20..23) replayed once each
   (read-only and build/test calls at their base commits) stored at most 1,515,236 bytes per
   session, and at most 3,229,745 counting the skipped full-gate and timed-out calls at their
   measured upper bounds, so the cap holds about 80 such sessions. Reaching a cap stops
   storing, never the command.
8. **Honesty.** The shell result names the handle only when `capture` is `complete`,
   `stored-cap-reached` or `storage-incomplete`, and says which. On `storage-failed` it says recovery is
   unavailable. The finish gate is unchanged: a stored or recovered output is never a
   verification run.

## Consequences

- Host plumbing beyond the list in Evidence: `HostDeps` carries the session-wide store
  (`crates/p1-host/src/lib.rs`), and the workspace fingerprint (parent and worker stall
  watch, `crates/p1-host/src/catalog/children.rs`) skips only the store's own run directory
  (`crates/p1-host/src/fingerprint.rs`), otherwise a session file inside a git workspace would
  make every shell command count as a file change for the finish gate.

- Nothing the shell cuts is lost any more, up to the configured disk caps; the model pages
  it instead of rerunning.
- The `tool` world gains its first new interface since the freeze: `wit.md` and
  `capabilities.md` tables, `LINKABLE_CAPABILITIES` (16 → 17), `Services`, the linker arm,
  the loader grant check, host wiring for the shell and `read_output`, and boundary tests all
  change together; a `wasm-boundary-v1.2` tag marks the merge.
- Session files gain a sibling directory; tools and people that copy or delete a session
  must include `FILE.outputs/`.
- Disk use grows with command output until the caps; the caps are chosen from measurement.
- Only the shell produces outputs in this round. `read` and `grep` page natively already;
  adding them later needs no interface change.

## Alternatives considered

- Store after filtering or after the guest squeeze: rejected, the host has already dropped
  the middle by then.
- Keep full output in memory: rejected, unbounded memory for a long build log.
- Add a handle field to `process-event`: rejected, it changes an existing record of a frozen
  interface; `produced()` gives the guest the handle additively.
- Paths or journal offsets as handles: rejected, a guest could name files; opaque host ids
  follow the worker-id precedent.
- Store in the journal JSONL: rejected, it would put megabytes of output into the file
  resume replays and the summariser reads.
- A global store under `$XDG_STATE_HOME`: rejected for session runs, lifetime would not
  follow the session; kept in mind for a later retention policy.
- iris's post-tool offload (results over 50 KiB stored after the tool returns): rejected, by
  then p1's host has already cut the middle.

## Evidence

- Cuts: `crates/p1-module-runtime/src/process/mod.rs` (`HEAD_BYTES`, `TAIL_BYTES`,
  `Capture::push`, `take_rest`), `crates/p1-module-runtime/src/process/stream.rs` (each chunk
  seen once), `crates/p1-tool-shell/guest/src/lib.rs` (`squeeze`).
- Redaction entry points: `crates/p1-redact/src/lib.rs` (`redact_with`, `held_suffix_len`,
  `shown_len`, `SecretSet`); per-agent secrets via `crates/p1-assembly/src/lib.rs`
  `ToolServices.mask`.
- Plumbing a tool-class capability touches: `modules/wit/worlds.wit` (`world tool`),
  `modules/capabilities.toml`, `crates/p1-module-runtime/src/loader.rs`
  (`LINKABLE_CAPABILITIES`, grant check), `crates/p1-module-runtime/src/capabilities.rs`
  (`Services`, `capability_linker`), `crates/p1-host/src/catalog/tools.rs`,
  `crates/p1-module-tests/tests/finish_boundary.rs` (grant tests to mirror).
- Donors: RTK `src/core/tee.rs`, iris `src/tools/read_output.rs`, `src/tools/handles.rs`.
- Measurements required before acceptance: #510 definition of done items 5 and 6.
