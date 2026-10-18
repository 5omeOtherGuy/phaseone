# Contributing to phaseone

Thank you for helping. This file covers how a change gets in. For what phaseone is and how it is built, read the [README](README.md). For the decisions behind it, read the ADRs in [`docs/adr/`](docs/adr/README.md). Agents and maintainers follow [`AGENTS.md`](AGENTS.md); where this file is shorter, that one holds.

## Ways to contribute

- Bug fixes and new features
- Build and CI improvements
- Documentation
- Triaging issues, and answering questions in issues

Small fixes are welcome as a pull request directly. Larger changes start with an issue, so the design can be agreed before you build it. phaseone is pre-release: issues and pull requests are triaged on a best-effort basis.

## Getting started

You need:

- The Rust toolchain. CI pins 1.98.1 (see `.github/actions/gate-setup/action.yml`).
- bash, git and python3.
- For the WebAssembly modules: the wasm32 target and the wasm-tools version pinned in `modules/toolchain.pins`. `scripts/module-toolchain.sh --check` verifies both.

In a new checkout, run `scripts/local-cargo-config.sh` once. It writes an untracked `.cargo/config.toml` that gives the checkout its own build target. Never commit that file.

## Checks

- The gate is `scripts/gate.sh`: formatting, clippy with warnings denied, the module build, the tests, and the checks on the scripts. It runs in CI on GitHub's runners (`.github/workflows/ci.yml`, whose required check is `gate`) for every pull request and every push to `main`. You do not run it in full locally.
- `scripts/pre-push.sh` runs the fast parts of the gate your change can break: formatting, clippy when Rust changed, and the script tests when `scripts/`, `.github/`, `docs/adr/` or `AGENTS.md` changed. Run the focused tests of the code you changed first (`cargo test -p <crate> <name>`); the gate runs the full tests of touched packages, and `P1_PREPUSH_TESTS=1` runs them locally too. Run it once before you push.

Some pull requests take a shorter gate on GitHub. One in which every changed file is Markdown outside any `environments/` directory runs only the ADR check and the secret scan; Markdown under `environments/` is prompt input and runs the full gate. One in which every changed file is under `packs/` is checked with the latest release instead of a build.

## Coding standards

- Rust: rustfmt and clippy, with warnings denied. The gate enforces both.
- Schema files: `crates/p1-module-protocol/tests/schema_bundle.rs` checks every fixture against its schema. Add a fixture when you change a schema.
- Markdown and JSON: match the style of the file you edit. JSON object keys are checked by `scripts/check-json-order.sh`. Markdown is not auto-formatted.

## Decisions and documentation

- A changed public interface or contract (the protocol and the wire format included), a new dependency, or a reversed decision gets an ADR in `docs/adr/`, in the same pull request as its code.
- Create one with `scripts/adr.py new "Title"`. It takes the next free number and writes the record as `proposed`; set its `status` to `accepted` before you open the pull request, because the record lands accepted with its code. To reverse an accepted record, write a new one with `scripts/adr.py new "Title" --supersedes N`; an accepted record changes only in `status` and `superseded_by`. Run `scripts/adr.py check` before you push.
- Ask before adding a crate the workspace does not use yet: say which and why in the issue.
- Document a feature before it ships to users, in the README or in `docs/`.

## Pull requests

1. Start from an up-to-date `main`.
2. Keep each pull request to one logical change.
3. Run `scripts/pre-push.sh`.
4. Open the pull request with a clear title and fill in the template.
5. CI runs the gate. `main` takes a change only when the `gate` check is green; that check is the only one branch protection requires.
6. Pull requests are squash-merged once `gate` is green, through auto-merge. The owner decides whether an outside pull request merges, and may ask for changes first.

## Asking for help

Ask on the issue you are working in. To report a bug, open an issue with the bug report form: the steps you ran, what you expected, and what happened. Report a security vulnerability privately, as [SECURITY.md](SECURITY.md) describes, never in a public issue.

## Code of Conduct

Everyone who takes part follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

phaseone is released under the [MIT License](LICENSE). By contributing, you agree that your contribution is licensed under the same terms, and that you have the right to submit it. There is no CLA.
