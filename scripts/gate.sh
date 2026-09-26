#!/usr/bin/env bash
# The one gate: native code and WebAssembly modules pass the same script, and `== gate: GREEN`
# is printed only after every step below has passed. No step is optional: a missing tool, a
# guest build or a validation failure ends the gate red, and no environment variable turns a
# step off. Live provider checks are NOT part of the gate (they need P1_LIVE=1 and the lead).
#
# Steps, in order:
#   fmt                 native and module workspace formatting
#   clippy              native clippy, warnings denied
#   modules toolchain   the pinned toolchain (scripts/module-toolchain.sh --check)
#   guest check         the module workspace's clippy for the pinned target, warnings denied
#   modules             optimized component builds (scripts/build-modules.sh --all, release profile)
#   module validation   every built package is complete, a valid component, matches its digest
#                       and world, and imports no more than its manifest's capability allocation
#                       (scripts/check-module-boundaries.sh --output-dir modules/target/p1-modules)
#   bubblewrap          the boundary tests' own bwrap probe: red outside CI when it fails
#   test                native tests, including the Wasmtime integration and conformance tests
#   core isolation      scripts/check-core-isolation.sh
#   module boundary     imports against the frozen capability allocation, and the unsafe policy
#                       (scripts/check-module-boundaries.sh, freeze items 11 and 13)
#   secret scan, adr, installer and CI helpers
#
# The module build comes before the tests so the integration and conformance tests exercise the
# components this commit builds, never stale or missing ones; they read them from
# modules/target/p1-modules/, where the build writes them. The bubblewrap boundary tests are
# required on a stream box and skip only on GitHub-hosted runners (ADR-0077), where `CI` is set.
#
# Exit 0 after `== gate: GREEN`; any other exit status is the failing step's own, and GREEN
# is not printed.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
# Local machines share one target dir across worktrees (disk + serialised builds); CI has none.
if [ -z "${CI:-}" ] && [ ! -f .cargo/config.toml ]; then scripts/local-cargo-config.sh; fi
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"

# The pinned wasm target, parsed like scripts/module-toolchain.sh parses the pins: they are
# data, and a stray command in them must not run.
wasm_target=""
while IFS= read -r line || [ -n "$line" ]; do
  line="${line%$'\r'}"
  case "$line" in WASM_TARGET=*) wasm_target="${line#WASM_TARGET=}" ;; esac
done <modules/toolchain.pins
if [ -z "$wasm_target" ]; then
  echo "gate: WASM_TARGET is missing from modules/toolchain.pins" >&2
  exit 1
fi

# The same probe the boundary tests run before they decide to skip, so a probe that passes here
# means they run.
bwrap_usable() {
  bwrap --ro-bind / / --dev /dev --proc /proc true </dev/null >/dev/null 2>&1
}

# Every built package must be one this commit builds, complete, a valid component, and match
# the digest and world the build recorded (docs/design/modules/package.md, "Build outputs").
validate_modules() {
  local dir pkg out expected=() built=() problems=0
  for dir in modules/p1-module-*/; do
    [ -f "$dir/Cargo.toml" ] || continue
    grep -q '^\[package\.metadata\.p1-module\]' "$dir/Cargo.toml" || continue
    expected+=("$(basename "$dir")")
  done
  if [ -d modules/target/p1-modules ]; then
    for dir in modules/target/p1-modules/*/; do
      [ -d "$dir" ] && built+=("$(basename "$dir")")
    done
  fi
  if [ "$(printf '%s\n' "${expected[@]}" | LC_ALL=C sort)" != "$(printf '%s\n' "${built[@]}" | LC_ALL=C sort)" ]; then
    echo "module validation: built packages [${built[*]}] differ from the packages under modules/ [${expected[*]}]" >&2
    return 1
  fi
  for pkg in "${expected[@]}"; do
    out="modules/target/p1-modules/$pkg"
    # Per package, so a file missing from one package skips only that package's content checks:
    # every other package is still validated, and one run reports every finding.
    local pkg_problems=0
    for f in "$pkg.wasm" "$pkg.wit" "$pkg.sha256" "$pkg.imports" "$pkg.manifest.json"; do
      [ -f "$out/$f" ] || { echo "module validation: $out/$f is missing" >&2; pkg_problems=$((pkg_problems + 1)); }
    done
    if [ "$pkg_problems" -eq 0 ]; then
      wasm-tools validate "$out/$pkg.wasm" || { echo "module validation: $pkg.wasm is not a valid component" >&2; pkg_problems=$((pkg_problems + 1)); }
      (cd "$out" && sha256sum --quiet -c "$pkg.sha256") || { echo "module validation: $pkg.wasm does not match $pkg.sha256" >&2; pkg_problems=$((pkg_problems + 1)); }
      wasm-tools component wit "$out/$pkg.wasm" | cmp -s - "$out/$pkg.wit" || { echo "module validation: $pkg.wit is not the world of $pkg.wasm" >&2; pkg_problems=$((pkg_problems + 1)); }
      grep -q "\"digest\": \"sha256:$(cut -d' ' -f1 "$out/$pkg.sha256")\"" "$out/$pkg.manifest.json" || { echo "module validation: $pkg.manifest.json names another digest" >&2; pkg_problems=$((pkg_problems + 1)); }
    fi
    problems=$((problems + pkg_problems))
  done
  [ "$problems" -eq 0 ] || return 1
  echo "module validation: ${#expected[@]} package(s) valid"
}

echo "== gate: fmt"
cargo fmt --all -- --check
cargo fmt --manifest-path modules/Cargo.toml --all -- --check
echo "== gate: clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
echo "== gate: modules toolchain"
scripts/module-toolchain.sh --check
echo "== gate: guest check"
# No --all-targets: libtest harnesses are not built for a component target, and the guests are
# tested through the host's integration tests against the built components.
cargo clippy --manifest-path modules/Cargo.toml --workspace --locked --target "$wasm_target" -- -D warnings
echo "== gate: modules"
scripts/build-modules.sh --all
echo "== gate: module validation"
validate_modules
# The imports of the components just validated against their manifests' capability
# allocation, before the tests load them: a component that imports more than it declares
# must never reach a Wasmtime test.
scripts/check-module-boundaries.sh --output-dir modules/target/p1-modules
echo "== gate: bubblewrap"
if bwrap_usable; then
  echo "bubblewrap: usable; the boundary tests run"
elif [ -n "${CI:-}" ]; then
  echo "bubblewrap: unusable on this CI runner; the boundary tests skip (ADR-0077)"
else
  echo "gate: bwrap is unusable on this machine, and the bubblewrap boundary tests are required outside CI" >&2
  exit 1
fi
echo "== gate: test"
# A hung test must end the gate red, not hold it forever: one test binary once parked on a
# futex for 37 minutes with nobody watching (2026-09-23). An hour covers a cold build under
# the rustc semaphore plus every test; a green gate never comes near it.
timeout --foreground 3600 cargo test --workspace --locked
echo "== gate: core isolation"
scripts/check-core-isolation.sh
echo "== gate: module boundary"
# The tag's standing boundary check, next to core isolation: imports and the unsafe policy of
# every package and crate, over the outputs the tests ran against.
scripts/check-module-boundaries.sh
echo "== gate: secret scan"
scripts/secret-scan.sh
echo "== gate: adr"
scripts/adr.py check
python3 scripts/test_adr.py -q
echo "== gate: installer and CI helpers"
python3 scripts/test_ci_build.py -q
python3 scripts/test_fanout.py -q
python3 scripts/test_gate.py -q
python3 scripts/test_install.py -q
python3 scripts/test_local_cargo_config.py -q
python3 scripts/test_release_manifest.py -q
python3 scripts/test_run_report.py -q
python3 scripts/test_rustc_serial.py -q
python3 scripts/test_secret_scan.py -q
python3 scripts/test_stage_release.py -q
python3 scripts/test_usage_audit.py -q
# S7.5.2: the release-candidate smoke test and artifact staging go here, through
# scripts/stage-release.sh (S7.7).
target_dir="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
echo "== target dir: $(du -sh "$target_dir" 2>/dev/null | cut -f1) $target_dir (free: $(df -h --output=avail "$target_dir" | tail -1 | tr -d ' '))"
echo "== gate: GREEN"
