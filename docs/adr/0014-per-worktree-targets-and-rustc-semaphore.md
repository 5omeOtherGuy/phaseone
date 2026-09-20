---
adr: 14
title: Per-worktree cargo targets seeded by hardlinks and a global rustc semaphore
status: accepted
date: 2026-09-20
deciders: lead
supersedes: [13]
superseded_by: []
sources: [D20, D12, scripts/local-cargo-config.sh, scripts/rustc-serial, AGENTS.md, 4c38457]
---
# ADR-0014: Per-worktree cargo targets seeded by hardlinks and a global rustc semaphore

## Context

D20 (lead) supersedes the mechanism of D12 while keeping D12's goals (small SSD, bounded
RAM). The shared target directory was unsound: cargo names a workspace member's artifacts from
its path relative to the workspace root, so worktrees collided on the same files and freshness
was judged by mtime. A worker's gate linked another worktree's stale `unimplemented!` stub
(assembly handoff, run `.worker-runs/20260920-004…`, reported as an "environment flake").

## Decision

Each worktree builds into its OWN `target/`, seeded with `cp -al` hardlinks from the main
checkout's `../phaseone-target`, with every workspace-member artifact removed. RAM is bounded
by `scripts/rustc-serial`, a rustc wrapper that holds one of two machine-wide `flock` slots
for the life of each real compilation. `CARGO_TARGET_DIR` is never set.

## Consequences

Third-party dependencies cost no extra disk and are never rebuilt; only the small `p1-*`
crates compile per worktree. Compilation can wait for a machine-wide slot, so a build that
seems to hang must be waited for, not killed. `git worktree remove` frees the worktree's
target.

## Alternatives considered

Keeping the shared target directory. D20 rejected it after the false green; the owner's
D12 goals (disk, RAM) are still met by seeding plus the semaphore.

## Evidence

D20's measurements: seeding 0.4 s, 0 bytes extra disk for dependencies, 4 p1 crates
rebuilt in 3.6 s. `../../scripts/local-cargo-config.sh` and `../../scripts/rustc-serial`
carry the rationale in their header comments; `AGENTS.md` ("Build") states the rules. Commit
4c38457.
