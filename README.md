# phaseone-connectors

The subscription connectors for [p1](https://github.com/5omeOtherGuy/phaseone): the routes,
logins and usage probes that reach the Claude Code and Codex subscriptions, OpenCode Zen and Go,
ClinePass, and the GLM and Kimi coding plans. They are p1 module packages built with p1's module
SDK, outside phaseone, as any module author's are (phaseone ADR-0210, ADR-0215).
`subscriptions/README.md` describes what is here.

## Install

Every release carries one package archive per module, the catalog archive (routes and profiles)
and `SHA256SUMS`. Install a package into a released p1:

    p1 modules install connectors-claude-code-oauth.p1pkg --confirm-origins

p1's distribution composes one release of this repository, pinned by tag and digest in
phaseone's `connectors.lock` (`scripts/fetch-connectors.py --pin <tag>`): its share carries the
catalog and its release set the packages.

## Build and check

    cargo fmt --all -- --check
    cargo clippy --workspace --locked --target wasm32-unknown-unknown -- -D warnings
    cargo test --locked -p p1-subscription-logins -p p1-subscription-usage
    cargo test --manifest-path subscriptions/tests/Cargo.toml --locked
    ./release.sh        # every package into dist/, with the catalog archive and SHA256SUMS
    ./conformance.sh    # p1's author conformance checks on each archive in dist/

`release.sh` and `conformance.sh` run phaseone's own build and checks from the checkout cargo
fetched for `p1-module-sdk`; they need Rust 1.98.1 with the `wasm32-unknown-unknown` target and
`wasm-tools` at that checkout's pin (`modules/toolchain.pins`). CI (`.github/workflows/ci.yml`)
runs all of it on every push and, on a tag `v<version>`, publishes the release.

## Versions

The SDK and the host crates the suites use are pinned once, at one phaseone revision (root
`Cargo.toml`, `subscriptions/tests/Cargo.toml`). A package declares the WIT world it was built
at; a p1 host loads it when it serves that world's major at an equal or newer minor (ADR-0210).
