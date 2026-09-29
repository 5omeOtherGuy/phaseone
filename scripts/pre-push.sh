#!/usr/bin/env bash
# The local checks before a push (ADR-0107): the parts of scripts/gate.sh a change can break,
# run on this machine so a pull request's one `gate` run on GitHub finds nothing new.
#
#   fmt       both workspaces, always
#   clippy    the native workspace, warnings denied, --keep-going: every crate's findings in
#             one run; the module workspace too when modules/ changed
#   modules   scripts/build-modules.sh --all before the tests, which load the built components
#   test      `cargo test --no-fail-fast` for the packages whose files changed: every failing
#             test in one run, not the first failing binary
#   scripts   scripts/adr.py check and every scripts/test_*.py, when scripts/, .github/,
#             docs/adr/ or AGENTS.md changed (the script tests pin texts of those files)
#
# Every step runs even after one fails; the script exits 1 if any failed and prints each
# step's result and seconds. The changed files are those of `git diff <base>...HEAD` plus the
# working tree; <base> is origin/main unless given as $1. The full gate stays GitHub's.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"
base="${1:-origin/main}"
git fetch -q origin main 2>/dev/null || true
mapfile -t changed < <({ git diff --name-only "$base"...HEAD; git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u)
[ "${#changed[@]}" -gt 0 ] || { echo "pre-push: nothing changed against $base"; exit 0; }

failed=0
summary=()
step() {
  local name="$1"; shift
  local start=$SECONDS
  echo "== pre-push: $name"
  if "$@"; then summary+=("ok    $name $((SECONDS - start)) s")
  else summary+=("FAIL  $name $((SECONDS - start)) s"); failed=1; fi
}
touches() { printf '%s\n' "${changed[@]}" | grep -qE "$1"; }

# The package of each changed file under crates/: the deepest manifest directory above it.
mapfile -t packages < <(cargo metadata --no-deps --format-version 1 --locked \
  | jq -r --arg root "$PWD/" '.packages[] | "\(.manifest_path | ltrimstr($root) | rtrimstr("Cargo.toml"))\t\(.name)"' \
  | awk -F'\t' 'NR == FNR { dir[$1] = $2; next }
      { best = ""; for (d in dir) if (index($0, d) == 1 && length(d) > length(best)) best = d
        if (best != "") print dir[best] }' - <(printf '%s\n' "${changed[@]}") | sort -u)

step fmt sh -c 'cargo fmt --all -- --check && cargo fmt --manifest-path modules/Cargo.toml --all -- --check'
if touches '\.rs$|Cargo\.(toml|lock)$'; then
  step clippy cargo clippy --workspace --all-targets --locked --keep-going -- -D warnings
fi
if touches '^modules/'; then
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
