#!/usr/bin/env bash
# Builds the channel shell plugin (E5) and the channel fixture plugin twice (its normal manifest, and one naming a capability the
# catalog lacks) as wasm32-unknown-unknown components, the way scripts/build-modules.sh builds
# a module: the module workspace's release profile, then `wasm-tools component new`, no WASI.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
target="$(cargo metadata --manifest-path modules/Cargo.toml --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
out=modules/target/p1-channel
mkdir -p "$out"
core="$target/wasm32-unknown-unknown/release/p1_module_channel_fixture.wasm"
for variant in default unknown-capability; do
  features=()
  [ "$variant" = default ] || features=(--features "$variant")
  scripts/build-admission.sh
  cargo build --manifest-path modules/Cargo.toml -p p1-module-channel-fixture --release \
    --target wasm32-unknown-unknown "${features[@]}"
  wasm-tools component new "$core" -o "$out/channel-fixture-$variant.wasm"
  wasm-tools validate "$out/channel-fixture-$variant.wasm"
done
scripts/build-admission.sh
cargo build --manifest-path modules/Cargo.toml -p p1-module-channel-shell --release \
  --target wasm32-unknown-unknown
wasm-tools component new "$target/wasm32-unknown-unknown/release/p1_module_channel_shell.wasm" \
  -o "$out/channel-shell.wasm"
wasm-tools validate "$out/channel-shell.wasm"
wasm-tools component wit "$out/channel-fixture-default.wasm"
ls -l "$out"
