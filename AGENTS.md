# p1 project rules

Read `~/.agents/AGENTS.md`.
Use `docs/design/README.md` for design, `DECISIONS.md` for settled choices and GitHub Issues for work.
Let only the lead edit `STATUS.md`.
Read `docs/worker-observability.md`, `docs/lead-queue.md` and `docs/iris-workflow.md` when working on those programmes.

## Work ownership and landing

Use the global task/worktree claim procedure before dispatch or creation.
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
Commit often on the task branch; keep branches short-lived (no long-lived branches; main has no branch protection).
Follow the global commit/PR/review/repair/merge order and merge as soon as review and the gate are green; do not bypass review with a local direct-to-main merge.
A small diff is a mergeable diff; resolve conflicts without breaking either accepted behavior, then rerun the relevant gate.
Remove a finished worktree with `git worktree remove <path>` only after authorized ownership release and preservation of its work.

## Workers

Use `scripts/fanout.py <jobs.json>` under the global routing policy; it starts jobs up to a machine-wide pool bound and prints one JSON summary when the batch ends.
A job with `"runner": "p1"` runs through p1 itself, with full access by default; `"sandbox":true` confines its shell.
Inspect the run directory's journal, stdout/stderr and `report.json` from `scripts/run-report.py`.
Verify independently and record accepted dogfood runs in `docs/dogfood/runs.jsonl`.
Follow model-cards for briefing, nonblocking supervision, repairs and evidence.

## Gate and decisions

Run `scripts/gate.sh` before merge: fmt check, clippy with `-D warnings`, all tests and core isolation.
Treat it as the required automated suite, alongside independent review and exact-commit CI; CI runs exactly the same script.
Intermediate commits need not run the full gate.
After merging main, use `scripts/push-main.sh` and verify the CI run for exactly that commit.
Do not equate a green workstation gate with green CI; CI lacks bubblewrap and can start more slowly.
Create an ADR for changed interfaces, dependency/workflow rules or reversed decisions using `scripts/adr.py new "Title"`.
Keep it proposed until merged, then accepted or rejected.
Change accepted ADRs only in `status` and `superseded_by`; reverse a decision with a new ADR using `--supersedes N`.
Keep small choices in commit messages; `scripts/adr.py check` runs in the gate.
Reconcile obsolete SSD-target instructions in ADR-0060 with the owner order through the ADR process.

## Project safety

Use no sudo or package installs; raise the need in an issue.
Never read, print, log, commit or put into fixtures any credential, token, Authorization header, private prompt or raw authenticated traffic.
Use no live network in unit/conformance tests.
Use tempfile/scratch data, never real user data directories.
Use fake time or explicit synchronization, not sleep-based timing assertions.
Treat `/home/phaseonebig/projects/iris-agent` and `/home/phaseonebig/projects/iris-agent-clean` as read-only donors.
Copy and adapt donor code into p1; name its donor path in the commit message.
Never delete, weaken or skip frozen acceptance tests or fixtures; leave a spec-conflicting test failing and explain.
Keep dependencies few; ask before adding a crate absent from the workspace.

## Build

The machine has a small SSD and 7 GB RAM (global rules: two build jobs, SSD floor).
Use a distinct task target under the global `/mnt/build/cargo-target/` root.
Never share another checkout's target; D20 records stale linking of worktree p1 crates.
Keep `scripts/rustc-serial`; its machine-wide semaphore admits at most two rustc processes.
Wait for a slot; do not kill a waiting build or bypass the wrapper.
Verify `scripts/local-cargo-config.sh` and the worktree helper honor the current HDD target rather than obsolete checkout-local target settings.
Use `cargo check -p <crate>` or `cargo test -p <crate> <filter>` while iterating, then the full gate at the integration boundary.
Use no release build, cargo install, extra toolchain or target unless the task authorizes it.
Do not move or remove a running build's target.

## Architecture

Keep `p1-core` to the loop and API, depending only on `p1-contracts`.
Keep providers, tools, file formats, prompt templates and UI names out of p1-core.
Make each tool a separate module/crate.
Keep providers limited to wire translation, with no tools.
Keep provider wire formats and UI types out of tools.
Expose only assembled prompts and tools to an agent; an unassembled tool cannot dispatch.
Keep delegation optional, with no mandatory coordinating agent.
Compose at compile time with ordinary constructors; add no plugin loader, service locator, global registry or DI framework.
Make public async interfaces Send-capable; give each agent's mutable state one owner.
Represent unknown usage/cost as None, never zero.
Use Rust 2024; forbid unsafe; use thiserror for library errors.
Use descriptive names, not mythology names.
Comments explain why, not what; add no speculative abstraction or unused generality.
