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
Land every slice as the owner's landing order requires (`~/.agents/OWNER-ORDERS.md` `<landing>`, skill `pr-pipeline`; ADR-0107): `scripts/pre-push.sh`, push, `gh pr create --fill`. The lead decides whether the PR gets a review: at most one review round, run locally with `scripts/review-pr.sh <pr> <focus-file>` (read-only `codex exec`), whose focus file names what to check and what to leave; none at skill level L1 (up to 50 changed lines in one or two files of one package), the lead's call at L2, one at L3 to L5, and none at any level when the lead or another agent already reviewed the change. One repair round takes the gate's failures and the confirmed P0/P1 findings; nothing is reviewed again, and lesser findings go into one follow-up issue. Then `gh pr merge --auto --squash --delete-branch --match-head-commit <sha>`, so the merge follows the green gate; do not bypass the gate with a local direct-to-main merge.
A small diff is a mergeable diff; resolve conflicts without breaking either accepted behavior, then rerun the relevant gate.
Remove a finished worktree with `git worktree remove <path>` only when its work is merged or pushed and its board claim is released by its owner or the lead.

## Workers

Use `scripts/fanout.py <jobs.json>` under the global routing policy; it starts jobs up to a machine-wide pool bound and prints one JSON summary when the batch ends.
A job with `"runner": "p1"` runs through p1 itself, with full access by default; `"sandbox":true` confines its shell. Fleet workers never use it: they run in pi or opencode (`~/.agents/OWNER-ORDERS.md` `<workers_not_p1_20260927>`).
Inspect the run directory's journal, stdout/stderr and `report.json` from `scripts/run-report.py`.
Verify independently and record accepted dogfood runs in `docs/dogfood/runs.jsonl`.
Follow model-cards for briefing, nonblocking supervision, repairs and evidence.

## Gate and decisions

The gate is `scripts/gate.sh`, and CI runs it: fmt check, clippy with `-D warnings`, all tests and core isolation; run the whole gate on no workstation or build box (ADR-0105); before a push run only `scripts/pre-push.sh` (ADR-0107).
Before a merge, the PR's CI (which runs exactly this script) must be green and the review the lead chose, if any, must have no open P0/P1 finding.
Intermediate commits need not run the full gate.
After a PR merges, verify main's own `gate` run for exactly the merge commit; direct pushes to main (`scripts/push-main.sh`) are refused.
Do not equate a green workstation gate with green CI; CI provisions bubblewrap too (ADR-0097) and can start more slowly.
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

Build and test locally (owner 2026-09-30, ADR-0107): before a push, run `scripts/pre-push.sh`: fmt, the workspace clippy, the modules, `cargo test --no-fail-fast` for the packages the change touches and the script tests when scripts, workflows, ADRs or this file changed; every step runs and one run reports every defect.
GitHub Actions runs only the required `gate` check (`.github/workflows/ci.yml`) on pull requests and main; there is no task-branch build farm and no downloaded binary.
The machine has a small SSD and 11 GiB usable RAM (global rules: three build jobs, SSD floor).
A local build target goes on the SSD, one per task: `CARGO_TARGET_DIR=~/.cache/cargo-target/<task>`, `CARGO_BUILD_JOBS=3`, at most three concurrent rustc and three concurrent builds, none started under 1.2 GiB MemAvailable (D25; `scripts/pre-push.sh` waits through `scripts/build-admission.sh`), while the SSD has 12 GiB free to admit a new build (8 GiB to keep building); below that admission the target is `/data/build/<task>` on the internal HDD (ext4, the data tier; `~/.agents/OWNER-ORDERS.md` `<builds_and_storage>`, owner 2026-09-29 23:30).
The target belongs to the task and its owner deletes it at task end; never share a target between checkouts (D20 records stale linking of worktree p1 crates).
`scripts/local-cargo-config.sh` (run by the worktree helper) defaults to `~/.cache/cargo-target/<checkout>-<hash>`, accepts an explicit `CARGO_TARGET_DIR` below `~/.cache/cargo-target` or `/data/build`, and refuses a non-ext4 target; the `/mnt/build` target stays retired (owner order 2026-09-25 02:40); `/data/build/<task>` is the data-tier path above.
`scripts/local-cargo-config.sh` writes an untracked `.cargo/config.toml`; never commit it.
Keep `scripts/rustc-serial`; its machine-wide semaphore admits at most three rustc processes.
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
Use Rust 2024; forbid unsafe (one exception, ADR-0113: the loader's deserialization of a release's verified compiled component); use thiserror for library errors.
Use descriptive names, not mythology names.
Comments explain why, not what; add no speculative abstraction or unused generality.
