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
| D6 | 2026-09-19 | lead | Installed rustup components `rustfmt` and `clippy` for the stable toolchain (user-level, `~/.rustup`, no apt/sudo). | The required gate (fmt check + clippy -D warnings) could not run: `cargo-fmt is not installed for the toolchain stable`. Not speculative; nothing system-wide changed. |
