# Contributing to phaseone

Thank you for helping. This file covers how a change gets in. For what phaseone is and how it is built, read the [README](README.md). For the decisions behind it, read the [ADR index](docs/adr/README.md).

## Ways to contribute

- Bug fixes and new features
- Build and CI improvements
- Documentation
- Triaging issues, and answering questions in issues and discussions

## Getting started

You need:

- The Rust toolchain. CI pins 1.98.1 (see `.github/workflows/ci.yml`).
- bash, git and python3.
- For the WebAssembly modules: the wasm32 target and the wasm-tools version pinned in `modules/toolchain.pins`. `scripts/module-toolchain.sh --check` verifies both.

In a new checkout, run `scripts/local-cargo-config.sh` once. It writes an untracked `.cargo/config.toml` that gives the checkout its own build target. Never commit that file.

## Checks

- `scripts/gate.sh` runs the full gate that CI runs: formatting, clippy with warnings denied, the module build, the tests, and the checks on the scripts. It is slow, so run it when you change shared code.
- `scripts/pre-push.sh` runs the fast parts of the gate your change can break: formatting, clippy and the script tests. Run the focused tests of the code you changed first (`cargo test -p <crate> <name>`); the gate runs the full tests of touched packages, and `P1_PREPUSH_TESTS=1` runs them locally too. Run it once before you push.

A pull request in which every changed file is Markdown runs a shorter gate on GitHub: the ADR check and the secret scan.

## Coding standards

- Rust: rustfmt and clippy, with warnings denied. The gate enforces both.
- Schema files: `crates/p1-module-protocol/tests/schema_bundle.rs` checks every fixture against its schema. Add a fixture when you change a schema.
- Markdown and JSON: match the style of the file you edit. JSON object keys are checked by `scripts/check-json-order.sh`. Markdown is not auto-formatted.

## Decisions and documentation

- A decision that shapes the design gets an ADR in `docs/adr/`. Create one with `scripts/adr.py new "Title"`. It takes the next free number and starts as `proposed`. Run `scripts/adr.py check` before you push.
- A change to the protocol or the wire format needs an accepted ADR before implementation starts. Open the ADR as `proposed`, get it reviewed and accepted, then implement.
- Document a feature before it ships to users, in the README or in `docs/`. Record the decision in an ADR when the feature settles one.

## Pull requests

1. Start from an up-to-date `main`.
2. Keep each pull request to one logical change.
3. Run `scripts/pre-push.sh`.
4. Open the pull request with a clear title and fill in the template.
5. CI runs the gate. The `gate` check must be green.
6. A maintainer reviews the change and may ask for changes. Once it is approved, a maintainer merges it.

## Asking for help

Ask on the issue or discussion you are working in. To report a bug, open an issue from the template in `.github/ISSUE_TEMPLATE/`, with the steps you ran, what you expected, and what happened.

## Code of Conduct

Everyone who takes part follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

phaseone is released under the [MIT License](LICENSE). By contributing, you agree that your contribution is licensed under the same terms, and that you have the right to submit it.
