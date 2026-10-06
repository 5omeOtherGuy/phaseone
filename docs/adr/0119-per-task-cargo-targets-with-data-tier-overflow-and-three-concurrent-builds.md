---
adr: 119
title: Per-task Cargo targets with data-tier overflow and three concurrent builds
status: proposed
date: 2026-10-06
deciders: owner+lead
supersedes: [60]
superseded_by: []
sources: [docs/adr/0060-one-shared-cargo-target-per-repository-across-worktrees-rustc-serial-ignores-slots-whose-holder-is-stopped.md, scripts/local-cargo-config.sh, scripts/rustc-serial, scripts/build-admission.sh, scripts/pre-push.sh, DECISIONS.md]
---
# ADR-0119: Per-task Cargo targets with data-tier overflow and three concurrent builds

## Context

ADR-0060 recorded the build-storage rules of 2026-09-25 but stayed `proposed`, and three of
its rules were overtaken by later owner orders before it could be accepted (issue #577,
AGENTS.md "Reconcile obsolete SSD-target instructions in ADR-0060 with the owner order"):

- It calls the internal HDD "not a build location". The owner's order of 2026-09-29 23:30
  made the HDD the data tier, `/data`, with `/data/build/<task>` as the build overflow when
  the SSD is below admission (fleet rules, system-maintenance `<rust_builds>`).
- It keeps two `rustc` slots and two Cargo jobs. D24 (owner, 2026-09-29) raised both to three.
- It does not bound concurrent builds. D25 (owner, 2026-10-01) allows at most three, none
  started under 1.2 GiB MemAvailable.

Its other rules (one target per task, D20 isolation, never stop a build, never move a running
target, CI as the release builder) still hold and are carried over unchanged.

## Decision

1. **One Cargo target per task, never shared.** `scripts/local-cargo-config.sh` writes the
   untracked `.cargo/config.toml` selecting an ext4 target owned by that checkout; its owner
   deletes it at task end. D20's stale-link failure is why it is never shared.
2. **SSD first, data tier when the SSD is short.** A new target goes below
   `~/.cache/cargo-target/<task>` on the SSD while the SSD has 12 GiB free (8 GiB to keep
   building); below that admission it goes to `/data/build/<task>` on the internal HDD
   (`P1_BUILD_HDD`, ext4). A non-ext4 target is refused.
3. **Three slots, three builds, a memory floor.** `scripts/rustc-serial` admits at most three
   concurrent `rustc` processes (`P1_RUSTC_SLOTS`, default 3) and Cargo uses three jobs (D24).
   At most three Cargo builds run at once and none starts under 1.2 GiB MemAvailable;
   `scripts/pre-push.sh` waits through `scripts/build-admission.sh` (D25). A slot is taken
   only by acquiring its `flock`, never inferred from holder metadata.
4. **Never stop or move a running build.** No SIGSTOP of a Cargo build or worker tree; a
   target is moved or removed only after its process tree has stopped.
5. **CI builds releases and runs the gate.** The full gate runs in CI only (ADR-0105); before a
   push only `scripts/pre-push.sh` runs (ADR-0107). Releases are built by
   `.github/workflows/release.yml` after a green `main` gate (ADR-0065).

## Consequences

- A task can always build: when the SSD is short its target moves to the slower HDD instead of
  blocking. HDD builds are slower; the speed difference is unmeasured here.
- Up to three builds share 11 GiB RAM; the 1.2 GiB floor and the swap alarm are the checks.
- ADR-0060 is superseded; ADR-0014's per-worktree isolation and global semaphore remain the
  foundation, its hardlink seeding does not describe the current layout.

## Alternatives considered

- **Keep ADR-0060 and amend it:** not allowed; an ADR changes only in `status` and
  `superseded_by`, and a reversed decision gets a new ADR (AGENTS.md).
- **Accept ADR-0060 as written:** would record two rules the owner reversed (HDD retired,
  two slots).

## Evidence

- `scripts/local-cargo-config.sh`: SSD root `~/.cache/cargo-target`, data-tier root
  `P1_BUILD_HDD` (default `/data/build`), ext4 check.
- `scripts/rustc-serial`: `slots="${P1_RUSTC_SLOTS:-3}"`.
- DECISIONS.md D20, D24, D25; owner storage order 2026-09-29 23:30 (system-maintenance
  `<rust_builds>`).
- Lead run 2026-10-06: SSD at 12 GiB free, follow-up targets for #577, #586, #588 and #556
  placed in `/data/build/<task>` by `scripts/new-worktree.sh` with an explicit
  `CARGO_TARGET_DIR`.
