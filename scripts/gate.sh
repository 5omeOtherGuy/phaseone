#!/usr/bin/env bash
# Local gate: format check, clippy with warnings denied, all tests.
# Green is required for every completed increment and for final acceptance.
# Live provider checks are NOT part of the gate (they need P1_LIVE=1 and the lead).
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
# Local machines share one target dir across worktrees (disk + serialised builds); CI has none.
if [ -z "${CI:-}" ] && [ ! -f .cargo/config.toml ]; then scripts/local-cargo-config.sh; fi
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"

echo "== gate: fmt"
cargo fmt --all -- --check
echo "== gate: clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
echo "== gate: modules"
scripts/module-toolchain.sh --check
scripts/build-modules.sh --all
scripts/check-module-boundaries.sh
echo "== gate: test"
# A hung test must end the gate red, not hold it forever: one test binary once parked on a
# futex for 37 minutes with nobody watching (2026-09-23). An hour covers a cold build under
# the rustc semaphore plus every test; a green gate never comes near it.
timeout --foreground 3600 cargo test --workspace --locked
echo "== gate: core isolation"
scripts/check-core-isolation.sh
echo "== gate: secret scan"
scripts/secret-scan.sh
echo "== gate: adr"
scripts/adr.py check
python3 scripts/test_adr.py -q
python3 scripts/test_ci_build.py -q
python3 scripts/test_fanout.py -q
python3 scripts/test_install.py -q
python3 scripts/test_local_cargo_config.py -q
python3 scripts/test_release_manifest.py -q
python3 scripts/test_run_report.py -q
python3 scripts/test_rustc_serial.py -q
python3 scripts/test_secret_scan.py -q
python3 scripts/test_usage_audit.py -q
target_dir="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
echo "== target dir: $(du -sh "$target_dir" 2>/dev/null | cut -f1) $target_dir (free: $(df -h --output=avail "$target_dir" | tail -1 | tr -d ' '))"
echo "== gate: GREEN"
