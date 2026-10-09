# p1 project rules

Read `~/.agents/AGENTS.md`. Design: `docs/design/README.md`; settled choices: `docs/adr/` (`DECISIONS.md` is the frozen first ledger); work: GitHub Issues. Only the lead edits `STATUS.md`. Read `docs/worker-observability.md`, `docs/lead-queue.md` and `docs/iris-workflow.md` only when working on those programmes.

## Commands

- New task checkout: `scripts/new-worktree.sh <issue>-<slug>` creates `../phaseone-<issue>-<slug>` on branch `task/<issue>-<slug>` with its own SSD target.
- Focused test: `cargo test -p <crate> <name>`.
- Before the one push of a pull request: `scripts/pre-push.sh` (fmt; modules before selected package tests; tests of touched packages and packages whose source/tests/build.rs read changed shipped-data directories, derived from path references; script tests when `scripts/`, `.github/`, `docs/adr/` or this file changed). Root-manifest-only changes select no package. Clippy runs in CI; `P1_PREPUSH_CLIPPY=1` adds it locally.
- Open and land: `gh pr create --fill`, then `gh pr merge --auto --squash --delete-branch --match-head-commit <sha>`.
- After the merge: `scripts/retire-worktree.sh ../phaseone-<issue>-<slug>` removes the worktree, its branch and its target; it refuses a dirty tree or an unmerged branch.
- Decision record: `scripts/adr.py new "Title"`; `scripts/adr.py check` runs in the gate.

## How work moves (owner 2026-10-08, ADR-0128)

- One worker owns one issue end to end: worktree, code, tests, self-review, pre-push, pull request, auto-merge, retirement. Opus at medium or high effort by default. The lead dispatches and verifies the merge; nothing in between.
- Up to three issues in flight at once, on disjoint files. Check `git worktree list` before creating a worktree; resume a task's existing worktree.
- Review before the first push: the worker runs one read-only reviewer agent on its diff and fixes P0 and P1 findings before `scripts/pre-push.sh`. P2 and P3 findings go in the pull request body. After the push the gate is the only check; no review round, no repair round.
- Small changes accumulate into the next pull request on the same area; a status or record change never gets its own pull request when a code pull request is due the same day.
- A docs-only pull request (every changed file is Markdown) takes the gate's fast path: no Rust jobs, about a minute.
- A decision record only for a changed public interface or contract, a dependency or workflow rule, or a reversed decision; it lands `accepted` in the pull request that lands its code. No record before a slice; no separate accept pull request. Change accepted records only in `status` and `superseded_by`; reverse with `--supersedes N`. Take the next free number and check unmerged worktrees for numbers already taken.
- Worker brief: at most ten lines (issue, owned paths, definition of done with its commands, what not to touch). No evidence rows, stage tables or decision-log files.
- Owner questions: one batched entry in `~/.agents/xo/for-owner.md`; proceed on the least risky option labelled as an assumption.
- Claim an issue by assignment and `ready` to `in-progress`; `blocked` with a comment naming the need; `owner` is reserved for owner decisions.
- Commit by explicit owned path; never `git add -A`; never force-push main. Main requires the `gate` check (D23): changes reach main only through a pull request whose gate is green; `scripts/push-main.sh` is refused. Do not equate a green workstation run with green CI: CI provisions bubblewrap (ADR-0097) and its sandbox suites must run there.
- Shared files (`Cargo.toml`, `Cargo.lock`, `scripts/`, `.github/`, this file): merge current main immediately before touching them; keep the edit minimal; change only owned paths otherwise. Never run fleet workers in p1.

## Build

- Everything on the SSD: worktrees under `~/projects/`, targets under `~/.cache/cargo-target/<task>` written by `scripts/local-cargo-config.sh` into an untracked `.cargo/config.toml` (never committed). No `/data/build` targets. Never share a target between checkouts (D20).
- Retire a worktree and its target the moment its pull request merged; a stale worktree is a defect of its owner.
- Machine limits: `scripts/rustc-serial` (three rustc slots machine-wide), `CARGO_BUILD_JOBS=3`, at most three concurrent builds and none under 1.2 GiB MemAvailable (`scripts/build-admission.sh`, D25). Wait for a slot; never kill a waiting build or move a running build's target.
- No release build, cargo install, extra toolchain or target locally. CI's `release.yml` builds the one release after a green gate on main (ADR-0065).
- The gate is `scripts/gate.sh` (fmt, clippy `-D warnings`, all tests, core isolation); CI runs it (`.github/workflows/ci.yml`); run it in full nowhere else (ADR-0105).

## Architecture

- Keep `p1-core` to the loop and API, depending only on `p1-contracts`; providers, tools, file formats, prompt templates and UI names stay out of it.
- `p1-tui` and its driver `crates/p1-host/src/tui.rs` are frozen (owner 2026-10-09): a parts donor, not p1's TUI. Do not extend, improve, redesign or plan work on them, and do not base new front-end work on them or on `docs/design/tui/`; copy useful pieces into new code and name the source path in the commit. When another change breaks their build or tests, restore them with the smallest mechanical edit that changes no behaviour; nothing else. The owner decides p1's new front end.
- Each tool is its own module or crate; providers do wire translation only, with no tools; provider wire formats and UI types stay out of tools.
- Expose only assembled prompts and tools to an agent; an unassembled tool cannot dispatch. Delegation stays optional, with no mandatory coordinating agent.
- Compose explicitly at one root: the host loads WebAssembly modules by name from the environment file (ADR-0071); no service locator, global registry, auto-registration or DI framework.
- Public async interfaces are Send-capable; each agent's mutable state has one owner; unknown usage or cost is None, never zero.
- Rust 2024; forbid unsafe (one exception, ADR-0113); thiserror for library errors; descriptive names, not mythology; comments explain why; no speculative abstraction.

## Safety

- No sudo and no package installs; raise the need in an issue.
- Never inspect, print, log, commit or put into fixtures credential values, tokens, Authorization headers, private prompts or raw authenticated traffic.
- No live network in unit or conformance tests; tempfile or scratch data, never real user directories; fake time or explicit synchronization, never sleep-based timing.
- `~/projects/iris-agent` and `~/projects/iris-agent-clean` are read-only donors: copy and adapt, naming the donor path in the commit message.
- Never delete, weaken or skip frozen acceptance tests or fixtures; leave a spec-conflicting test failing and explain.
- Keep dependencies few; ask before adding a crate absent from the workspace.
