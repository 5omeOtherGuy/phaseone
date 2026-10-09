#!/usr/bin/env bash
# The local checks before a push (ADR-0107): the parts of scripts/gate.sh a change can break,
# run on this machine so a pull request's one `gate` run on GitHub finds nothing new.
#
#   fmt       both workspaces, always
#   clippy    the native workspace, warnings denied, --keep-going: every crate's findings in
#             one run; the module workspace too when modules/ changed
#   modules   scripts/build-modules.sh --all before the tests, which load the built components
#   test      `cargo test --no-fail-fast` for changed packages and packages whose Rust
#             source/tests/build.rs reference changed shipped-data dirs (p1-module-tests
#             when modules/ changed): every failing test in one run, not the first failing binary
#   scripts   scripts/adr.py check and every scripts/test_*.py, when scripts/, .github/,
#             docs/adr/ or AGENTS.md changed (the script tests pin texts of those files)
#
# Every step runs even after one fails; the script exits 1 if any failed and prints each
# step's result and seconds. The changed files are those of `git diff <base>...HEAD` plus the
# working tree; <base> is origin/main unless given as $1. The full gate stays GitHub's.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"
base="${1:-origin/main}"
# Builds go through the machine-wide rustc semaphore (scripts/rustc-serial, three slots) that
# scripts/local-cargo-config.sh writes into .cargo/config.toml: however many sessions run this at
# once, their compilations wait for a slot instead of running beside each other.
grep -qs 'rustc-serial' .cargo/config.toml || {
  echo "pre-push: .cargo/config.toml names no rustc-serial wrapper; run scripts/local-cargo-config.sh first" >&2
  exit 2
}
git fetch -q origin main 2>/dev/null || true
git rev-parse --verify -q "$base^{commit}" >/dev/null || { echo "pre-push: base $base is not a commit" >&2; exit 2; }
list="$({ git diff --name-only "$base"...HEAD && git diff --name-only HEAD && git ls-files --others --exclude-standard; } | sort -u)" \
  || { echo "pre-push: cannot list the changed files against $base" >&2; exit 2; }
mapfile -t changed <<<"$list"
[ -n "$list" ] || { echo "pre-push: nothing changed against $base"; exit 0; }

failed=0
summary=()
step() {
  local name="$1"; shift
  local start=$SECONDS
  echo "== pre-push: $name"
  if "$@"; then summary+=("ok    $name $((SECONDS - start)) s")
  else summary+=("FAIL  $name $((SECONDS - start)) s"); failed=1; fi
}
# No `grep -q`: under pipefail its early exit can fail printf with SIGPIPE and read as no match.
touches() { printf '%s\n' "${changed[@]}" | grep -E "$1" >/dev/null; }

# Select direct owners plus shipped-data readers from the code, not a package list.
# modules/ still adds p1-module-tests; root manifests alone still select no package.
selection="$(cargo metadata --no-deps --format-version 1 --locked \
  | python3 scripts/pre_push_packages.py "${changed[@]}")" \
  || { echo "pre-push: cannot map changed files to test packages" >&2; exit 2; }
packages=()
if [ -n "$selection" ]; then mapfile -t packages <<<"$selection"; fi

# Before the first compiling step: at most three builds on the machine and 1.2 GiB MemAvailable
# (owner 2026-10-01, D25); it waits, saying why, and never fails.
if touches '\.rs$|Cargo\.(toml|lock)$|^modules/' || [ "${#packages[@]}" -gt 0 ]; then
  scripts/build-admission.sh
fi
step fmt cargo fmt --all -- --check
step "guest fmt" cargo fmt --manifest-path modules/Cargo.toml --all -- --check
# Clippy runs in CI (ADR-0128); P1_PREPUSH_CLIPPY=1 adds it here.
if [ "${P1_PREPUSH_CLIPPY:-0}" = 1 ] && touches '\.rs$|Cargo\.(toml|lock)$'; then
  step clippy cargo clippy --workspace --all-targets --locked --keep-going -- -D warnings
fi
if [ "${P1_PREPUSH_CLIPPY:-0}" = 1 ] && touches '^modules/'; then
  step "guest clippy" cargo clippy --manifest-path modules/Cargo.toml --workspace --locked --keep-going \
    --target wasm32-unknown-unknown -- -D warnings
fi
if [ "${#packages[@]}" -gt 0 ]; then
  step modules scripts/build-modules.sh --all
  args=()
  for p in "${packages[@]}"; do args+=(-p "$p"); done
  step "test ${packages[*]}" cargo test --locked --no-fail-fast "${args[@]}"
fi
if touches '^scripts/|^\.github/|^docs/adr/|^AGENTS\.md$'; then
  step adr python3 scripts/adr.py check
  for t in scripts/test_*.py; do step "$t" python3 "$t" -q; done
fi

echo "== pre-push: summary (against $base)"
printf '%s\n' "${summary[@]}"
exit "$failed"
