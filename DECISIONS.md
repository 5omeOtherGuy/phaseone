# Decisions ledger

Every decision with its reason. `[owner]` = owner decision (do not change; material
changes go to `OWNER-QUESTIONS.md`). `[lead]` = technical decision by the orchestrating
session, may be revised with evidence (append a new entry, do not rewrite history).

| # | Date | Kind | Decision | Reason |
|---|---|---|---|---|
| D1 | 2026-09-19 | owner | p1 is a new project; Iris is a parts donor. One small core + modules; tools are their own modules; providers translate only; harness reshapes around the model (Claude and GPT first); delegation optional; Rust, lean, descriptive names; first slice already modular. | Handoff §2, discovery notes. |
| D2 | 2026-09-19 | lead | New repo at `projects/phaseone`, branch `main` holds only the initial scaffold commit; all work on task branch `slice-1`. No remotes, no pushes. | Handoff §3: work on a task branch, no merge into main without the owner. |
| D3 | 2026-09-19 | lead | License MIT (copied from iris-agent `LICENSE`, same owner). Iris `NOTICE` lists Codex-derived Apache-2.0 files only under `src/ui/tui/streaming/*`, which p1 does not take; any donor file carrying an SPDX Apache header keeps it and gets a `NOTICE` entry. | Handoff §6 license check; iris-agent@62c8345 `NOTICE`. |
| D4 | 2026-09-19 | lead | Crate prefix `p1-`; one crate per module boundary under `crates/`. Core isolation is enforced by `scripts/check-core-isolation.sh` on the resolved `cargo tree`, inside the gate. | seams.md §10 first acceptance item must be a runnable command, not a convention. |
| D5 | 2026-09-19 | lead | Live provider checks require `P1_LIVE=1`, are run only by the lead, and are not part of the gate. | Handoff §3 secrets/live rules. |
| D6 | 2026-09-19 | owner | p1 is developed in the open as a public GitHub repo `5omeOtherGuy/phaseone`. This supersedes D2's "no remotes, no pushes": development is trunk-based on `main`, with short-lived `task/*` branches merged as soon as the gate is green. No review gate, no branch protection. | Owner direction to optimise the repo for rapid development by several agents at once. |
| D7 | 2026-09-19 | lead | CI is one job that runs `scripts/gate.sh`, on pushes to `main` and on pull requests. There are no other workflows, no matrices and no separate review/audit bots; the gate script stays the single definition of green. | D6; one check that cannot drift from the local gate. |
| D8 | 2026-09-19 | lead | Task state lives in GitHub Issues (labels `ready`, `in-progress`, `blocked`, `owner`); each task gets its own git worktree via `scripts/new-worktree.sh`. `STATUS.md` and `OWNER-QUESTIONS.md` are removed — they duplicated the board and every agent edited them, so they conflicted rather than informed. | Swarm coordination needs one shared truth per concern; files every agent rewrites are contention points. |
| D9 | 2026-09-19 | lead | The design baseline (`pillars.md`, `design-summary.md`, `seams.md`) is copied into `docs/design/` so the public repo is self-contained; `projects/phaseone-collab/` stays the working copy outside it. | D6: public readers cannot follow links to the owner's workstation. |
| D6 | 2026-09-19 | lead | Installed rustup components `rustfmt` and `clippy` for the stable toolchain (user-level, `~/.rustup`, no apt/sudo). | The required gate (fmt check + clippy -D warnings) could not run: `cargo-fmt is not installed for the toolchain stable`. Not speculative; nothing system-wide changed. |
