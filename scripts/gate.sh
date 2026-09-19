#!/usr/bin/env bash
# Local gate: format check, clippy with warnings denied, all tests.
# Green is required for every completed increment and for final acceptance.
# Live provider checks are NOT part of the gate (they need P1_LIVE=1 and the lead).
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"

echo "== gate: fmt"
cargo fmt --all -- --check
echo "== gate: clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
echo "== gate: test"
cargo test --workspace --locked
echo "== gate: core isolation"
scripts/check-core-isolation.sh
echo "== gate: GREEN"
