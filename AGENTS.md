# p1 project rules

Read `~/.agents/AGENTS.md`.
Use `docs/design/README.md` for design, `DECISIONS.md` for settled choices and GitHub Issues for work.
Let only the lead edit `STATUS.md`.
Read `docs/worker-observability.md`, `docs/lead-queue.md` and `docs/iris-workflow.md` when working on those programmes.

## Work ownership and landing

Before dispatch or creating a worktree, check `git worktree list`, the board's claims and the worktree inventory, then claim the path (`board-me claim --path <dir>`; no-duplicate-work order in `~/.agents/OWNER-ORDERS.md`).
Resume the task's existing worktree; create a new one only when the task has none.
Use `scripts/new-worktree.sh <task-slug>` for a new task checkout at `../phaseone-<task-slug>`.
Use task branches named `task/<issue>-<slug>`; verify the helper's generated branch.
Claim an issue by assignment and `ready` → `in-progress`; when blocked, add `blocked` and comment with the specific need.
Reserve the `owner` label for owner decisions.
Change only owned paths; do not reformat, rename or tidy another task's files.
Keep shared-file edits minimal: `AGENTS.md`, `DECISIONS.md`, `Cargo.toml`, `Cargo.lock`, `scripts/`, `.github/`.
Merge current main immediately before touching shared files.
Keep `DECISIONS.md` append-only.
Stage and commit only explicit owned paths; never `git add -A`; never force-push main.
Commit often on the task branch; keep branches short-lived (no long-lived branches).
main requires the `gate` check (branch protection, admins included, owner 2026-09-25): a change reaches main only through a PR whose gate is green, so `gh pr merge --auto` waits for green.
Land every slice as the owner's landing order requires (`~/.agents/OWNER-ORDERS.md`, 2026-09-24 21:00): `gh pr create --fill`, an independent cheap review, repair until it approves, then `gh pr merge --auto --squash --delete-branch` on green PR CI; do not bypass review with a local direct-to-main merge.
A small diff is a mergeable diff; resolve conflicts without breaking either accepted behavior, then rerun the relevant gate.
Remove a finished worktree with `git worktree remove <path>` only when its work is merged or pushed and its board claim is released by its owner or the lead.

## Workers

Use `scripts/fanout.py <jobs.json>` under the global routing policy; it starts jobs up to a machine-wide pool bound and prints one JSON summary when the batch ends.
A job with `"runner": "p1"` runs through p1 itself, with full access by default; `"sandbox":true` confines its shell.
Inspect the run directory's journal, stdout/stderr and `report.json` from `scripts/run-report.py`.
Verify independently and record accepted dogfood runs in `docs/dogfood/runs.jsonl`.
Follow model-cards for briefing, nonblocking supervision, repairs and evidence.

## Gate and decisions

Run `scripts/gate.sh` before merge: fmt check, clippy with `-D warnings`, all tests and core isolation.
Before a merge, the PR's CI (which runs exactly this script) and an independent review must both be green.
Intermediate commits need not run the full gate.
After a PR merges, verify main's own `gate` run for exactly the merge commit; direct pushes to main (`scripts/push-main.sh`) are refused.
Do not equate a green workstation gate with green CI; CI lacks bubblewrap and can start more slowly.
Create an ADR for changed interfaces, dependency/workflow rules or reversed decisions using `scripts/adr.py new "Title"`.
Keep it proposed until merged, then accepted or rejected.
Change accepted ADRs only in `status` and `superseded_by`; reverse a decision with a new ADR using `--supersedes N`.
Keep small choices in commit messages; `scripts/adr.py check` runs in the gate.
Reconcile obsolete SSD-target instructions in ADR-0060 with the owner order through the ADR process.

## Project safety

Use no sudo or package installs; raise the need in an issue.
Never inspect, print, log, commit or put into fixtures credential values, tokens, Authorization headers, private prompts or raw authenticated traffic; p1's own authentication code may read its designated credential source at runtime, keeping values out of agent context and logs.
Use no live network in unit/conformance tests.
Use tempfile/scratch data, never real user data directories.
Use fake time or explicit synchronization, not sleep-based timing assertions.
Treat `/home/phaseonebig/projects/iris-agent` and `/home/phaseonebig/projects/iris-agent-clean` as read-only donors.
Copy and adapt donor code into p1; name its donor path in the commit message.
Never delete, weaken or skip frozen acceptance tests or fixtures; leave a spec-conflicting test failing and explain.
Keep dependencies few; ask before adding a crate absent from the workspace.

## Build

CI is the build farm: push the task branch and run `scripts/ci-build.sh`; green is the run of exactly that commit, and the downloaded `ci-artifacts/<sha>/p1` passes `sha256sum -c` against the uploaded `p1.sha256`.
Local cargo is ONLY `cargo check -p <crate>`, plus the lead's deployed-binary rebuild.
A `task/**` push runs `scripts/gate.sh` and builds `p1` (debug) in `.github/workflows/build.yml`, uploading `dist/p1`, its sha256 and the gate log as the `p1-build` artifact.
The machine has a small SSD and 7 GB RAM (global rules: two build jobs, SSD floor).
Any local build target goes on the SSD, one per task: `CARGO_TARGET_DIR=~/.cache/cargo-target/<task>`, `CARGO_BUILD_JOBS=2`, at most two concurrent rustc, and only above the SSD floor (8 GiB free to keep building; 12 GiB to admit a new build).
The target belongs to the task and its owner deletes it at task end; never share a target between checkouts (D20 records stale linking of worktree p1 crates).
`scripts/local-cargo-config.sh` (run by the worktree helper) defaults to `~/.cache/cargo-target/<checkout>-<hash>` and refuses a non-ext4 target; the `/mnt/build` HDD target is retired (owner order 2026-09-25 02:40).
`scripts/local-cargo-config.sh` writes an untracked `.cargo/config.toml`; never commit it.
Keep `scripts/rustc-serial`; its machine-wide semaphore admits at most two rustc processes.
Wait for a slot; do not kill a waiting build or bypass the wrapper.
Use no release build, cargo install, extra toolchain or target unless the task authorizes it.
The one release build is CI's: `.github/workflows/release.yml` builds `p1` in release profile on a GitHub runner after a green `gate` on main and publishes `main-<shortsha>`, so a user installs without a toolchain (ADR-0065).
Do not move or remove a running build's target.

## Architecture

Keep `p1-core` to the loop and API, depending only on `p1-contracts`.
Keep providers, tools, file formats, prompt templates and UI names out of p1-core.
Make each tool a separate module/crate.
Keep providers limited to wire translation, with no tools.
Keep provider wire formats and UI types out of tools.
Expose only assembled prompts and tools to an agent; an unassembled tool cannot dispatch.
Keep delegation optional, with no mandatory coordinating agent.
Compose explicitly at one root: the host loads WebAssembly modules by name from the environment file (ADR-0071); add no service locator, global registry, auto-registration or DI framework.
Make public async interfaces Send-capable; give each agent's mutable state one owner.
Represent unknown usage/cost as None, never zero.
Use Rust 2024; forbid unsafe; use thiserror for library errors.
Use descriptive names, not mythology names.
Comments explain why, not what; add no speculative abstraction or unused generality.
