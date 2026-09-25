---
adr: 60
title: Per-task SSD Cargo targets and the machine-wide rustc limit
status: proposed
date: 2026-09-23
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/adr/0014-per-worktree-cargo-targets-seeded-by-hardlinks-and-a-global-rustc-semaphore.md, scripts/local-cargo-config.sh, scripts/rustc-serial, scripts/install.sh, README.md]
---
# ADR-0060: Per-task SSD Cargo targets and the machine-wide rustc limit

## Context

Two incidents on 2026-09-23 on the 117 GB / 7 GB machine that runs p1 and brain-tools
still constrain local builds:

1. **Disk.** Three live p1 worktrees held 21–27 GB of `target/` (8.8, 8.3 and 4.3 GB; one grew
   to 15 GB after a clippy `--all-targets` + test rebuild). Free space fell from 27 GB to 3 GB
   twice in one day; a full root filesystem broke every session in the morning. ADR-0014's
   seeding (hardlinks of the main checkout's third-party artifacts) shares dependencies, but
   each worktree still owns its workspace artifacts, test and example binaries, incremental
   caches and debuginfo.
2. **Stall.** Two SIGSTOPped worker process trees held both `rustc-serial` slots (a `flock` per
   slot held through `exec`), so every build on the machine waited at 0 % CPU until the stopped
   group was found.

This ADR originally proposed one target shared by all worktrees in a repository and a
metadata salt to keep same-named workspace crates apart. The owner's build-storage orders of
2026-09-25 superseded that layout. Builds that cannot run in the cloud use a distinct target
below `~/.cache/cargo-target` on the SSD, one per task; the internal HDD is not a build
location. p1 release builds run in CI and are installed from GitHub releases.

Cargo names a workspace member's artifacts from its path relative to the workspace root, so
one shared target also requires careful worktree isolation. D20 is the recorded failure where
one worktree silently linked another worktree's stale `p1-core` artifact. A per-task target
retains isolation without a salt experiment or hardlink seeding.

## Decision

1. **Keep one Cargo target per task on the SSD.** Every local build that cannot run in the
   cloud uses a distinct directory below `~/.cache/cargo-target/<task>`. Never share that
   target with another task or worktree. `scripts/local-cargo-config.sh` writes the
   untracked `.cargo/config.toml` that selects the per-checkout task target on ext4; this
   supersedes the proposed shared-repository target and per-worktree metadata salt.
2. **Admit a local build only with storage headroom.** Require at least 12 GiB free on the SSD
   to start, and preserve the 8 GiB SSD floor. A build that can run in the build farm does
   not run locally. This supersedes the proposed 10 GB / 4,096 MiB worktree and fanout
   thresholds for Cargo admission.
3. **Keep the two-compilation limit.** Local Cargo uses two jobs and
   `scripts/rustc-serial` holds a machine-wide semaphore around real compilations, admitting
   at most two concurrent `rustc` processes. The wrapper may identify and report stopped or
   stale holders, but it must never infer that a slot is available without acquiring its
   `flock`. This supersedes the proposal to ignore a stopped holder and proceed.
4. **Do not stop builds.** Never SIGSTOP a Cargo build or worker process tree to save disk.
   Let its current Cargo step finish, withhold the next step, and move only work that is not
   running. No slot is taken from a stopped holder, so a waiter waits until the lock is
   actually released.
5. **Use CI as the build farm.** A p1 release is built after a green `main` gate and installed
   with `scripts/install.sh --latest`; a local release build is only the fallback for a build
   that cannot run in the cloud. Ordinary local Rust verification is limited to the
   repository's focused check workflow. This supersedes the proposed per-worktree binary and
   artifact-directory layout.
6. **Do not move a running target.** Target trees are isolated before a build starts and are
   removed only after its process tree has stopped.

The parts of the original proposal that stand are the disk and stall constraints, the need to
keep workspace artifacts isolated (D20), the machine-wide two-`rustc` bound, and the rule
against stopping build process trees. The shared target, metadata salt, `--artifact-dir`
layout, line-table-debug profile change, reduced fanout threshold and stopped-holder bypass do
not stand.

## Consequences

- Local storage pressure is bounded per task rather than shared across worktrees, but a task
  cannot recover space by reusing another task's artifacts. The SSD and build-farm admission
  rules apply before local compilation starts.
- The D20 stale-link mode is prevented by directory isolation without changing Cargo metadata
  or depending on hardlink cleanup.
- At most two `rustc` processes run on the workstation. A stopped holder is diagnosed but its
  lock remains authoritative, so diagnosing a stall cannot violate the compilation bound.
- CI is the normal producer of p1 releases. Release installation needs no Rust toolchain; a
  fallback local build must obey the same SSD, admission and two-`rustc` rules.
- There is no shared-target salt spike or per-worktree binary layout to implement. Operators
  must create or preserve a distinct task target and must not move one during a build.
- ADR-0014's per-worktree isolation and global rustc semaphore remain the foundation, but
  its hardlink seeding and checkout-local storage do not describe the current target layout.

## Alternatives considered

- **One shared repository target with a worktree metadata salt:** protects workspace-member
  artifacts but still couples tasks' target trees, cleanup and disk growth. Superseded by the
  owner's per-task SSD order.
- **One target per checkout without a salt:** matches the current generated configuration and
  preserves D20 isolation through a new directory for each task. Selected.
- **Build on the internal HDD:** rejected by the owner; it is retired for builds.
- **`sccache`:** shares compilation results, not target trees, and does not replace task
  isolation, admission checks or the two-`rustc` limit.
- **Taking a slot from a stopped or apparently stale holder:** unsafe without a real lock;
  rejected. Holders are reported, and the kernel lock remains the authority.

## Evidence

- Owner build-storage orders of 2026-09-25: 12 GiB free and the 8 GiB floor to admit local
  builds; per-task `~/.cache/cargo-target/<task>` targets on the SSD; the internal HDD retired;
  p1 built by CI and installed from GitHub releases.
- `scripts/local-cargo-config.sh` selects a distinct ext4 target below
  `~/.cache/cargo-target`, sets `build.jobs = 2` and `incremental = false`, and installs the
  rustc wrapper in the untracked checkout configuration.
- `scripts/rustc-serial` uses stable `flock` slots (two by default), publishes holder metadata
  only after acquiring a slot, reports stopped or stale holders, and never admits a slot from
  holder metadata alone.
- `scripts/install.sh` makes `--latest` the default release mode. Its `--local` mode selects
  `$CARGO_TARGET_DIR` or `$HOME/.cache/cargo-target/p1-release`, refuses a non-ext4 target or
  one with less than 12 GiB free, and runs Cargo with two jobs. README.md records the normal
  release and SSD fallback workflows.
