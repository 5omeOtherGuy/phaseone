# p1 project rules

## Profile

- Purpose: a lean, modular Rust coding harness that adapts itself to the model it runs.
- Stage: pre-release (repository standard 2026-10-17)
- Gate: `scripts/gate.sh` (CI job `gate`)
- Landing: PR, squash-merged on a green gate (auto-merge); agents land: yes
- Release: manual dispatch of `release.yml`, tagged `main-<sha>` (ADR-0218); agents release: no
- Outside issues and PRs: agents triage; outside PRs merge: owner approves
- Deliberate deviations from the standard, each kept for the reason given:
  - Every change lands by pull request with auto-merge and the required `gate` check, also in pre-release (ADR-0010, ADR-0177): several sessions land in parallel, and the check is what keeps main green.
  - The gate keeps its name `scripts/gate.sh`, runs only on GitHub's runners, and `ci.yml` splits it into jobs whose results the `gate` job requires, with fast paths for docs-only, pack-only and reuse pull requests (ADR-0105, ADR-0128, ADR-0192, ADR-0206): only a gate run on GitHub's runners is evidence for a merge, and the builds are too heavy for a shared machine. `scripts/pre-push.sh` is the local check (ADR-0195).
  - A worktree per issue on `task/<issue>-<slug>`; the main checkout is never edited.
  - ADRs through `scripts/adr.py`, with a generated index (ADR-0191).
  - No local release builds; releases come from CI's `release.yml`.
  - `packs/owner/docs/workstation.md` holds the owner's workstation paths, so this file names none.
  - The owner approves new crates; frozen tests stay as they are (below).

p1 is a terminal coding harness in Rust: one agent core plus explicit modules. Design: `docs/design/README.md`; settled decisions: `docs/adr/`; capability seams: `docs/design/seams.md`; work: GitHub Issues.

## Landing work

- For issue work, pushing the task branch, opening its pull request and arming auto-merge are authorized (ADR-0010, ADR-0177): `gh pr create --fill`, then `gh pr merge --auto --squash --delete-branch --match-head-commit <sha>`. Done means merged.
- The repository is public: no commit, pull request body or squash message carries a personal email address. Strip a `Co-authored-by` trailer that names one, and give `gh pr merge` an explicit `--subject` and `--body` so the squash commit does not collect it.
- Main takes changes only through a pull request whose required `gate` check is green. The gate is `scripts/gate.sh` on GitHub's runners (`.github/workflows/ci.yml`); never run it in full locally. CI provisions bubblewrap, so its sandbox suites run there even where they skip on a workstation.
- Before the push, run the focused tests of the code you changed (`cargo test -p <crate> <name>`, the test binaries that pin the changed behaviour), then `scripts/pre-push.sh` once: fmt, clippy when Rust changed, and the script tests when `scripts/`, `.github/`, `docs/adr/` or this file changed. The full tests of touched packages run in the gate; `P1_PREPUSH_TESTS=1` runs them locally too (ADR-0195). A pull request whose changed files are all Markdown skips it and takes the gate's fast path; an `environments/**/prompt.md` change, the root's or a pack's, is not documentation and runs `cargo test -p p1-assembly` first. One whose changed files are all under `packs/` is gated by the latest release instead of a build (ADR-0206): run `p1 env check --dir packs/owner` and `P1_BIN=<p1> python3 packs/owner/tests/test_pack.py` before the push. A new head whose code is a clean merge of an earlier green head with main (merging main for a Markdown conflict or to catch up) reuses that green (ADR-0192).
- A user-visible change adds its entry under `## [Unreleased]` in `CHANGELOG.md`, in user-facing words and linking its pull request, in the same pull request.
- `.githooks/pre-push` runs `scripts/pre-push.sh` and refuses a direct push to main; enable it once per checkout with `git config core.hooksPath .githooks` (`.agents/setup` does it in an orb).
- Run as many issues in parallel as have disjoint files. One branch per issue: `scripts/new-worktree.sh <issue>-<slug>` creates `../phaseone-<issue>-<slug>` on `task/<issue>-<slug>`; resume an existing worktree (`git worktree list`). Once the pull request merged, `scripts/retire-worktree.sh ../phaseone-<issue>-<slug>` removes worktree, branch and target.
- The main checkout is shared, and other sessions read it as current main: never change files there. Create the task's worktree before the first edit, also for work without an issue (`scripts/new-worktree.sh <slug>`), and update the main checkout only on branch `main` with `git pull --ff-only origin main`.
- When the gate fails only in tests your diff does not touch and main fails them too, say so on the pull request and wait; once main is green, merge `origin/main` and push again.
- Decision records: `scripts/adr.py new "Title"` for a changed public interface or contract, a new dependency, or a reversed decision; it lands `accepted` in the pull request with its code. Accepted records change only in `status` and `superseded_by`; reverse one with `--supersedes N`. Take the next free number on `origin/main` and in open pull requests. The index is generated, not committed (`scripts/adr.py list` or `index`, ADR-0191); on a conflict in `docs/adr/README.md`, take main's copy.

## Local builds

- `scripts/new-worktree.sh` (or `scripts/local-cargo-config.sh <checkout>`) writes an untracked `.cargo/config.toml`: a target of its own and the machine-wide `scripts/rustc-serial` wrapper. Never commit it, never point two checkouts at the same target (cargo links stale `p1-*` crates across them), never bypass the wrapper. `scripts/pre-push.sh` waits for a free build slot by itself.
- Copying a warm target into a checkout's own target is permitted; cargo then rebuilds only what the copy does not hold fresh. `scripts/new-worktree.sh` does this from a template of dependencies without p1's own crates (`scripts/target-template.sh`), so a new worktree compiles only `p1-*`; `scripts/retire-worktree.sh` keeps the template current. Seed another empty target with `scripts/target-template.sh --seed <target>`. Never copy a target that still holds `p1-*` units, not even with `cp -a`: cargo hashes them by their path inside the workspace and reuses them as this checkout's own.
- No release builds, `cargo install` or extra toolchains locally; CI's `release.yml` builds releases.
- Focused test: `cargo test -p <crate> <name>`.
- Where these land on the owner's workstation (checkout and target paths, disk budgets, the donors): `packs/owner/docs/workstation.md`.

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
- A frozen test or fixture may take an edit the compiler forces on it, such as a match arm for a new variant or a value for a new struct field, when no assertion or expected value changes; the pull request names it. Anything else in a frozen file follows the rule above.
- The ACP TCK (`acp-tck`) is not part of the gate and never runs per pull request: it runs once on a release that contains ACP-surface changes and for the registry-listing check (#684). A pull request names the ACP tests it ran instead.
- No live network in unit or conformance tests; tempfile or scratch data, never real user directories; fake time or explicit synchronization, never sleep-based timing.
- Never inspect, print, log, commit or put into fixtures credential values, tokens, Authorization headers, private prompts or raw authenticated traffic.
- The iris-agent checkouts are read-only donors: copy and adapt, naming the donor path in the commit message.
