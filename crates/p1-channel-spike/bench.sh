#!/usr/bin/env bash
# Builds both guests and both benches, then runs the existing bench-baseline and the channel
# bench back to back. The channel crate gets opt-level 3 in the dev profile, as the workspace
# gives p1-module-runtime: wasmtime's generic lowering code is instantiated in the crate that
# calls it, so without this the channel's copies would run unoptimised and the existing
# runtime's would not. Passed with --config so no workspace file changes.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
scripts/build-admission.sh
scripts/build-modules.sh --package p1-module-fixture
crates/p1-channel-spike/build-guest.sh >/dev/null
scripts/build-admission.sh
cargo build --config 'profile.dev.package.p1-channel-spike.opt-level=3' \
  -p p1-module-tests --bin bench-baseline -p p1-channel-spike --bin bench-channel
target="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
echo "commit: $(git rev-parse HEAD)"
echo "rustc: $(rustc -V)"
echo "load: $(cut -d' ' -f1-3 /proc/loadavg)"
"$target/debug/bench-baseline"
"$target/debug/bench-channel"
