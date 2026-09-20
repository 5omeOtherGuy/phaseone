---
adr: 13
title: One shared cargo target directory across worktrees
status: superseded
date: 2026-09-19
deciders: owner+lead
supersedes: []
superseded_by: [14]
sources: [D12, D20, scripts/local-cargo-config.sh, AGENTS.md]
---
# ADR-0013: One shared cargo target directory across worktrees

## Context

D12 (owner+lead): a small SSD must not fill with Rust targets, and concurrent compiles
would exhaust 7 GB of RAM. The ledger records the owner's stated concern: "small SSD must
not fill with Rust targets, and concurrent compiles will exhaust 7 GB RAM", and the goal of
storing dependencies once so that "one compile at a time" is a property of the machine. D20
later found the shared directory unsound and superseded its mechanism while keeping the
goals.

## Decision

One shared target directory (`../phaseone-target`) for every checkout and worktree,
written into an untracked `.cargo/config.toml` by `scripts/local-cargo-config.sh`; `jobs = 2`,
incremental off, no debug info for dependencies. Cargo's build-directory lock serialised
compilation across agents. Independent test authors worked against supplied binaries and did
not build.

## Consequences

Dependencies were stored once and compilation was serialised by cargo's lock. The
mechanism turned out to be unsound: cargo derives a workspace member's artifact names from its
path relative to the workspace root, so every worktree's `p1-core` mapped to the same files
and freshness was judged by mtime. A stale `unimplemented!` stub from another worktree was
linked into a gate (the assembly handoff's "environment flake"). False greens were possible;
`main` was protected only by CI, which builds clean. See ADR-0014 for the replacement.

## Alternatives considered

A `target/` per worktree, as the handoff prescribed ("Each has its own `target/` (do not point
them at a shared `CARGO_TARGET_DIR`)") — set aside at the time because every worktree would
recompile and store all third-party dependencies. ADR-0014 returns to it with hardlink seeding.

## Evidence

D20 records the false green and names the assembly handoff run
`.worker-runs/20260920-004…`. `../../scripts/local-cargo-config.sh` and `AGENTS.md`
("Build") now describe the per-worktree layout instead. Commit 59e21c7 introduced the shared
directory; 4c38457 replaced it.
