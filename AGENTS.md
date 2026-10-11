# p1 project rules

p1 is a terminal coding harness in Rust: one agent core plus explicit modules. Design: `docs/design/README.md`; settled decisions: `docs/adr/`; capability seams: `docs/design/seams.md`; work: GitHub Issues.

## Landing work

- For issue work, pushing the task branch, opening its pull request and arming auto-merge are authorized (ADR-0010, ADR-0177): `gh pr create --fill`, then `gh pr merge --auto --squash --delete-branch --match-head-commit <sha>`. Done means merged.
- Main takes changes only through a pull request whose required `gate` check is green. The gate is `scripts/gate.sh` on GitHub's runners (`.github/workflows/ci.yml`); never run it in full locally. CI provisions bubblewrap, so its sandbox suites run there even where they skip on a workstation.
- Before the push, run `scripts/pre-push.sh` once: fmt, the tests of touched packages and of packages that read changed shipped data, and the script tests when `scripts/`, `.github/`, `docs/adr/` or this file changed; `P1_PREPUSH_CLIPPY=1` adds clippy. A pull request whose changed files are all Markdown skips it and takes the gate's fast path; an `environments/**/prompt.md` change is not documentation and runs `cargo test -p p1-assembly` first. A new head whose code is a clean merge of an earlier green head with main (merging main for a Markdown conflict or to catch up) reuses that green (ADR-0192).
- Run as many issues in parallel as have disjoint files. One branch per issue: `scripts/new-worktree.sh <issue>-<slug>` creates `../phaseone-<issue>-<slug>` on `task/<issue>-<slug>`; resume an existing worktree (`git worktree list`). Once the pull request merged, `scripts/retire-worktree.sh ../phaseone-<issue>-<slug>` removes worktree, branch and target.
- The main checkout (`~/projects/phaseone` on the workstation) is shared, and other sessions read it as current main: never change files there. Create the task's worktree before the first edit, also for work without an issue (`scripts/new-worktree.sh <slug>`), and update the main checkout only on branch `main` with `git pull --ff-only origin main`.
- When the gate fails only in tests your diff does not touch and main fails them too, say so on the pull request and wait; once main is green, merge `origin/main` and push again.
- Decision records: `scripts/adr.py new "Title"` for a changed public interface or contract, a new dependency, or a reversed decision; it lands `accepted` in the pull request with its code. Accepted records change only in `status` and `superseded_by`; reverse one with `--supersedes N`. Take the next free number on `origin/main` and in open pull requests. The index is generated, not committed (`scripts/adr.py list` or `index`, ADR-0191); on a conflict in `docs/adr/README.md`, take main's copy.

## Local builds

- `scripts/new-worktree.sh` (or `scripts/local-cargo-config.sh <checkout>`) writes an untracked `.cargo/config.toml`: a target of its own under `~/.cache/cargo-target/` and the machine-wide `scripts/rustc-serial` wrapper. Never commit it, never point two checkouts at the same target (cargo links stale `p1-*` crates across them), never bypass the wrapper. `scripts/pre-push.sh` waits for a free build slot by itself.
- Copying a warm target into a checkout's own target is permitted; cargo then rebuilds only what the copy does not hold fresh. `scripts/new-worktree.sh` does this: it hardlinks `~/.cache/cargo-target/p1-template`, the dependencies of a warm target without p1's own crates, so a new worktree compiles only `p1-*`. `scripts/target-template.sh [warm-target]` refreshes the template (default: the main checkout's target); run it after a warm target built a changed `Cargo.lock`. Copy a target by hand only with p1's crates dropped as that script does, or with `cp -a` (no hardlinks).
- A cold build (new target, no template) grows the SSD by 13 to 17 GiB: start one only with at least 25 GiB free (`df -h ~`).
- No release builds, `cargo install` or extra toolchains locally; CI's `release.yml` builds releases.
- Focused test: `cargo test -p <crate> <name>`.

## Architecture

- Modularity is what p1 is for (owner 2026-10-10, ADR-0153): a microkernel (`p1-core` is the agent loop and its API only, depending only on `p1-contracts`), ports and adapters (a port is a contract in `p1-contracts` or a WIT interface under `modules/wit/`; an adapter is a crate that implements it), Protected Variations, and dsh as the floor (every seam p1 builds is at least as modular as dsh's equivalent; a missing feature is not below the floor, a weaker seam is).
- Before building a capability, find its row in `docs/design/seams.md` and build the port named there. When the catalog or an accepted record says a part will vary (a source, provider, tool, front end or file format), give it its own contract and module in the first slice, even with one implementation; splitting it later is the expensive rewrite. A capability without a row gets one in the same pull request, and a pull request that builds or changes a capability updates its row.
- Providers, tools, file formats, prompt templates and UI names stay out of `p1-core`. Each tool is its own module; providers do wire translation only; provider wire formats and UI types stay out of tools.
- The host loads tools, providers and policies as WebAssembly modules by name from the environment file (ADR-0071), with no native fallback: the gate's shipping audit (`scripts/check-module-boundaries.sh --shipping`) fails on one. Compose explicitly at that one root: no service locator, global registry, auto-registration or DI framework.
- Expose only assembled prompts and tools to an agent; an unassembled tool cannot dispatch. Delegation stays optional, with no mandatory coordinating agent.
- Public async interfaces are Send-capable; each agent's mutable state has one owner; unknown usage or cost is `None`, never zero.
- Rust 2024; `unsafe` is forbidden (one exception, ADR-0113); thiserror for library errors; descriptive names, not mythology; comments explain why.
- `p1-tui` and its driver `crates/p1-host/src/tui.rs` are frozen (owner 2026-10-09): a parts donor, not p1's front end. Do not extend them or base new front-end work on them or on `docs/design/tui/`; copy useful pieces into new code and name the source path in the commit. When another change breaks them, restore them with the smallest mechanical edit. The owner decides p1's new front end.
- Ask the owner before adding a crate the workspace does not use yet.

## Tests and data

- Never delete, weaken or skip frozen acceptance tests or fixtures. Ask the owner before changing behavior one pins; a test that conflicts with the spec stays failing, with an explanation.
- No live network in unit or conformance tests; tempfile or scratch data, never real user directories; fake time or explicit synchronization, never sleep-based timing.
- Never inspect, print, log, commit or put into fixtures credential values, tokens, Authorization headers, private prompts or raw authenticated traffic.
- `~/projects/iris-agent` and `~/projects/iris-agent-clean` are read-only donors: copy and adapt, naming the donor path in the commit message.
