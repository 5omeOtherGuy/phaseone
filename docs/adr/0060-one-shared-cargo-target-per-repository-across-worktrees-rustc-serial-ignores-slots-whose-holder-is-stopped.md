---
adr: 60
title: One shared cargo target per repository across worktrees; rustc-serial ignores slots whose holder is stopped
status: proposed
date: 2026-09-23
deciders: owner+lead
supersedes: []
superseded_by: []
sources: [docs/adr/0014-per-worktree-cargo-targets-seeded-by-hardlinks-and-a-global-rustc-semaphore.md, scripts/local-cargo-config.sh, scripts/rustc-serial, scripts/new-worktree.sh, scripts/fanout.py, STATUS.md]
---
# ADR-0060: One shared cargo target per repository across worktrees; rustc-serial ignores slots whose holder is stopped

## Context

Two incidents on 2026-09-23 on the 117 GB / 7 GB machine that runs p1 and brain-tools:

1. **Disk.** Three live p1 worktrees held 21–27 GB of `target/` (8.8, 8.3 and 4.3 GB; one grew
   to 15 GB after a clippy `--all-targets` + test rebuild). Free space fell from 27 GB to 3 GB
   twice in one day; a full root filesystem broke every session in the morning. ADR-0014's
   seeding (hardlinks of the main checkout's third-party artifacts) already shares the
   dependencies; what fills the disk is each worktree's OWN artifacts: the workspace members'
   rlibs, every test and example binary of `--all-targets`, incremental caches and debuginfo.
   The owner asks for one shared `CARGO_TARGET_DIR` per repository across worktrees, for p1
   and for brain-tools.
2. **Stall.** The lead SIGSTOPped two worker process trees to save disk. Two stopped `rustc`
   processes kept both `rustc-serial` slots (an `flock` per slot, held through `exec`), so
   every build on the machine — three gates and the brain's — waited at 0 % CPU for three
   hours until XO found the stopped group. The semaphore has no notion of a holder that will
   never finish.

The constraint that shaped ADR-0014 still holds and is recorded in
`scripts/local-cargo-config.sh`: cargo names a workspace member's artifacts from its path
RELATIVE to the workspace root, so two worktrees' `p1-core` map to the same files in a shared
target and freshness is judged by mtime — on 2026-09-20 one worktree silently linked another's
stale `unimplemented!` stub (D20). Sharing must not bring that back.

## Decision

1. **One target directory per repository, shared by all its worktrees, with the workspace
   members kept apart by a per-worktree metadata salt.** `CARGO_TARGET_DIR` for every p1
   worktree is `../phaseone-target` (brain-tools: its equivalent); `scripts/local-cargo-config.sh`
   writes, per worktree, `[build] rustflags = ["-C", "metadata=<worktree-name>"]` restricted
   to the workspace members through `[profile.*.package."p1-*"]`-style scoping if cargo allows
   it, else through `CARGO_ENCODED_RUSTFLAGS` set only for member crates by `rustc-serial`
   (the wrapper sees `--crate-name` and the source path, and adds `-C metadata=<salt>` when the
   path is inside a worktree that is not the main checkout). The salt makes each worktree's
   member artifacts distinct files (`libp1_core-<hash>.rlib` differs) while every third-party
   crate — compiled without the salt — is one file shared by all. This replaces hardlink
   seeding and its `git worktree remove --force` cleanup. The stale-link failure of D20 cannot
   recur: same-named members from different worktrees never resolve to the same artifact.
2. **Uplifted binaries stay per checkout.** `target/debug/p1` is written only by the main
   checkout (`cargo build -p p1-host` there, as the landing procedure already requires); a
   worktree's `cargo build` of a binary goes to `target/<salt>/` via `--artifact-dir` when it
   becomes stable, and until then worktrees do not build binaries (they run `check`, `clippy`
   and `test`, whose test executables live in the hashed `deps/`).
3. **Shrink what every worktree still owns.** `[profile.dev] debug = "line-tables-only"` and
   `[profile.test] debug = "line-tables-only"` in the workspace `Cargo.toml` (backtraces keep
   file:line; variable-level debuginfo is not used by anyone here); `incremental = false` in
   `scripts/gate.sh` (a gate is a one-shot build; CI already runs so). Expected: a member
   rebuild of 8–15 GB becomes 3–6 GB, measured before acceptance.
4. **Guards.** `scripts/new-worktree.sh` refuses to create a worktree under 10 GB free and
   prints the top consumers; `scripts/fanout.py --min-free-mb` (exists) gets a default of
   4 096; the landing procedure's "remove the worktree" step stays.
5. **`rustc-serial` knows its holders.** After taking a slot the wrapper writes its pid into
   the slot file. A waiter that cannot take any slot reads each holder's `/proc/<pid>/stat`
   state; a holder in state `T` (stopped) or a dead pid is IGNORED — the waiter takes that slot
   as if free and prints one line `rustc-serial: slot N held by stopped pid P, proceeding` to
   stderr. A stopped rustc consumes no CPU or memory bandwidth, so the semaphore's purpose (at
   most two compilations RUNNING) is kept; when the stopped holder resumes, at most one extra
   compilation runs until it exits, which the 7 GB box tolerates for one process. A waiter
   still blocked after 10 minutes prints the holders (pid, state, command) once so a human
   sees a stuck build instead of silence.
6. **Rule, stated:** never SIGSTOP a build or a worker process tree; to hold a job back, let
   its current cargo step finish and withhold the next (STATUS lessons, 2026-09-23).

## Consequences

- Disk per extra worktree drops from "everything" to "the members, with line-table debuginfo",
  and third-party artifacts exist once per repository instead of once plus N hardlink forests.
- A stopped or dead build can no longer freeze the machine's builds; a stuck one is named.
- Cost: `local-cargo-config.sh` and `rustc-serial` change (both shared scripts, lead-owned);
  one Cargo.toml profile change; brain-tools adopts the same two scripts. Item 1's salt
  mechanism must be proven on this cargo version before the rest lands (a spike: two worktrees,
  one shared target, `p1-core` implemented in one and stubbed in the other, both gates green
  and the stub never linked into the implemented tree).
- ADR-0014 is superseded when this is accepted (its rustc semaphore survives, extended).

## Alternatives considered

- Plain shared `CARGO_TARGET_DIR` without a salt: the D20 incident; rejected.
- Keep per-worktree targets and only shrink debuginfo (item 3 alone): halves the problem,
  keeps the hardlink forests and the cleanup step; kept as the fallback if the salt spike fails.
- `sccache`: a new tool, and it shares compilations, not final artifacts; not needed for a
  single machine.
- `rustc-serial` killing a stopped holder: destroys someone else's build; ignoring the slot is
  enough.

## Evidence

Pending: the salt spike above; before/after `du` of a worktree target under items 1 and 3; a
reproduction of the stall with a stopped holder that a waiter now bypasses; gate and CI green.
Implementation after the quota resets.
