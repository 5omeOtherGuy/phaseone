---
adr: 55
title: A successful command that changes the workspace counts as progress for the stall guard
status: accepted
date: 2026-09-23
deciders: lead
supersedes: []
superseded_by: []
sources: [docs/adr/0037-unattended-runs-end-by-an-observable-finish-call-with-bounded-continuation.md, docs/adr/0042-a-headless-run-that-only-summarizes-ends-as-stalled.md, docs/design/completion.md, crates/p1-host/src/activity.rs, crates/p1-host/tests/stall_guard.rs]
---
# ADR-0055: A successful command that changes the workspace counts as progress for the stall guard

## Context

ADR-0037 records a stated limit: "a shell command that changes files is not seen as a file
change". ADR-0042's stall guard builds on the same view of progress (completion.md §3c): a tool
call with effect `WritesFiles`, or a `finish` call. On 2026-09-23 (workflows job 5, issue #53)
a Claude Opus 5.5 worker wrote its files through shell heredocs and `sed -i`; the guard saw six
context summaries with no `WritesFiles` call and ended a half-implemented, correct run as
`stalled`. The same blindness makes the `finish` check accept a verification command that ran
BEFORE a heredoc write, because that write never moved `last_file_change`. Telling models to
use the edit tool is a workaround that only works for models that obey. Counting every
successful command as progress is not acceptable: an agent looping on `ls` would never stall.

## Decision

1. **The host fingerprints the workspace after every successful `Executes` call.** In a git
   workspace the fingerprint is a hash over `git status --porcelain -uall` (paths respecting
   `.gitignore`, so `target/` and other build output never count) together with each listed
   path's size and mtime, so a second edit of an already-modified file is seen. Outside git it
   is a walk of the workspace excluding `.git`, `target`, `node_modules` and dot-directories.
   The fingerprint is computed once before the first command of a session and after each
   successful command; the comparison is the only thing that matters.
2. **A changed fingerprint is a workspace change.** It counts as progress for the stall guard
   (§3c) AND moves `last_file_change` for the `finish` check, exactly as a `WritesFiles` call
   does. The activity record notes `changed_workspace: true` on that command.
3. **An unchanged fingerprint changes nothing.** A successful `cargo test`, `ls` or `git status`
   is neither progress nor a file change, as today. A failed command is never fingerprinted.
4. **Bounded cost.** The fingerprint reads only what `git status` lists (or the walk's
   metadata); it never reads file contents. If it fails (no workspace, I/O error) the host
   falls back to today's rule and notes it once in the run report.
5. The stall message and the prompt's Finishing section stay as they are; ADR-0037's stated
   limit is amended: a command that changes tracked or untracked non-ignored files IS seen.

## Consequences

- A worker editing through the shell is judged by what it did to the workspace, not by which
  tool it used; #53's false stall cannot recur for that reason.
- The `finish` check becomes stricter in one honest way: a verification run followed by a
  heredoc write now requires the run to be repeated.
- Frozen tests of ADR-0037 and ADR-0042 are untouched (fake tools carry explicit effects); new
  tests cover the fingerprint path with a real temporary git workspace.

## Alternatives considered

- Prompt rule only ("edit with the edit tool"): free, but a model that ignores it is stalled
  again; kept as guidance, not as the mechanism.
- Any successful command counts: defeats the guard.
- Parsing commands for write patterns (`>`, `tee`, `sed -i`): brittle, and misses scripts.

## Evidence

Merged as e1850d2 (task/stall-fingerprint; DeepSeek V4.1 Flash worker, reviewed by the lead;
gate green, run recorded in `docs/dogfood/runs.jsonl`). Tests: `crates/p1-host/tests/
stall_fingerprint.rs` (a worker editing only through shell heredocs is not stalled; a worker
running only `ls` still is), `tests/finish_fingerprint.rs` (a verification run before a heredoc
write must be repeated; the changing command's own run counts and every earlier run is stale)
and the unit tests of `src/fingerprint.rs` (ignored paths, untracked files, a second edit, a
commit, the non-git walk, the entry bound, the host's own session journal, a missing
workspace). The frozen ADR-0037/0042 tests are untouched. Live, 2026-09-23, deepseek2 worker in
a temporary git workspace, shell tool only: `cat > notes.txt <<'EOF'` → `cat notes.txt` →
`finish done` accepted (exit 0), fingerprinting on (no fallback note), file content verified.
