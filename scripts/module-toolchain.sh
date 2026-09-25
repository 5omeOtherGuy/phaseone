#!/usr/bin/env bash
# Verifies and records the module toolchain against modules/toolchain.pins.
# The record is the evidence for the "recorded compatible toolchain" row of the S0 definition
# of done, so it runs on the stream boxes and in CI through scripts/gate.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
usage: scripts/module-toolchain.sh --check
       scripts/module-toolchain.sh --help

--check  verify the Rust toolchain, the wasm target and the pinned module tools against
         modules/toolchain.pins and print the toolchain record, one "key: value" per line.
Exit 0 when compatible, 1 when a check fails, 2 on a usage error.
EOF
}

if [ "$#" -ne 1 ]; then
  usage >&2
  exit 2
fi
case "$1" in
  --check) ;;
  --help | -h)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

pins_file=modules/toolchain.pins
problems=0
fail() {
  echo "module-toolchain: FAIL: $*"
  problems=$((problems + 1))
}

declare -A pin=()
# Parsed rather than sourced: the pins are data, and a stray command in them must not run.
if [ -f "$pins_file" ]; then
  lineno=0
  while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    line="${line%$'\r'}"
    case "$line" in '' | '#'*) continue ;; esac
    if [[ "$line" =~ ^([A-Z][A-Z0-9_]*)=([^[:space:]]+)$ ]]; then
      pin[${BASH_REMATCH[1]}]="${BASH_REMATCH[2]}"
    else
      fail "$pins_file:$lineno: not a KEY=value line"
    fi
  done <"$pins_file"
else
  fail "$pins_file missing"
fi

# Numeric major.minor.patch comparison; a pre-release suffix is ignored. Prints -1, 0 or 1.
version_cmp() {
  local a="${1%%-*}" b="${2%%-*}" i x y
  local -a av bv
  IFS=. read -r -a av <<<"$a"
  IFS=. read -r -a bv <<<"$b"
  for i in 0 1 2; do
    x="${av[i]:-0}"
    y="${bv[i]:-0}"
    if ((10#$x < 10#$y)); then echo -1; return; fi
    if ((10#$x > 10#$y)); then echo 1; return; fi
  done
  echo 0
}
is_version() { [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; }

# Every version of package $2 recorded in lockfile $1, space-separated.
lock_versions() {
  awk -v want="$2" '
    /^\[\[package\]\]/ { name = "" }
    /^name = / { name = $3; gsub(/"/, "", name) }
    /^version = / { v = $3; gsub(/"/, "", v); if (name == want) printf "%s ", v }
  ' "$1"
}

# Checks that pin $2 for package $1 is the version locked in every lockfile given after it,
# then prints its record line. Not called in a subshell, so its failures reach the count.
check_locked() {
  local package="$1" want="$2" lockfile found f result="$2"
  local -a failures=()
  shift 2
  for lockfile in "$@"; do
    found="$(lock_versions "$lockfile" "$package")"
    found="${found% }"
    if [ -z "$found" ]; then
      failures+=("$package $want pinned but absent from $lockfile")
      result="$want (FAIL: absent from $lockfile)"
    elif [[ " $found " != *" $want "* ]]; then
      failures+=("$package pinned $want but $lockfile has $found")
      result="$want (FAIL: $lockfile has $found)"
    fi
  done
  [ "$result" = "$want" ] && result="$want (matches $*)"
  echo "$package: $result"
  for f in "${failures[@]}"; do fail "$f"; done
}

# rustc and cargo
rustc_release=""
if command -v rustc >/dev/null 2>&1; then
  rustc_vv="$(rustc -vV)"
  rustc_release="$(sed -n 's/^release: //p' <<<"$rustc_vv")"
  rustc_hash="$(sed -n 's/^commit-hash: //p' <<<"$rustc_vv")"
  echo "rustc: $rustc_release ($rustc_hash)"
else
  echo "rustc: missing"
  fail "rustc not found on PATH"
fi
if command -v cargo >/dev/null 2>&1; then
  echo "cargo: $(cargo --version)"
else
  echo "cargo: missing"
  fail "cargo not found on PATH"
fi

rust_min="${pin[RUST_MIN]:-}"
if [ -z "$rust_min" ]; then
  echo "rust_min: not pinned"
  fail "RUST_MIN missing from $pins_file"
elif ! is_version "$rust_min"; then
  echo "rust_min: $rust_min"
  fail "RUST_MIN $rust_min is not a major.minor.patch version"
else
  echo "rust_min: $rust_min"
  if [ -n "$rustc_release" ]; then
    if ! is_version "$rustc_release"; then
      fail "rustc release $rustc_release is not a major.minor.patch version"
    elif [ "$(version_cmp "$rustc_release" "$rust_min")" = -1 ]; then
      fail "rustc $rustc_release is older than RUST_MIN $rust_min"
    fi
  fi
fi

# The target's std lives in the sysroot whether rustup installed it or not.
target="${pin[WASM_TARGET]:-}"
if [ -z "$target" ]; then
  echo "target: not pinned"
  fail "WASM_TARGET missing from $pins_file"
elif [ -n "$rustc_release" ] && [ -d "$(rustc --print sysroot)/lib/rustlib/$target" ]; then
  echo "target: $target (std installed)"
else
  echo "target: $target (std missing)"
  fail "the $target std is not installed in the rustc sysroot"
fi

if [ -n "${pin[WASMTIME]:-}" ]; then
  check_locked wasmtime "${pin[WASMTIME]}" Cargo.lock
else
  echo "wasmtime: not pinned yet (S0.2)"
fi

# Features are a record, not a lockfile fact: Cargo.lock does not carry them.
if [ -n "${pin[WASMTIME_FEATURES]:-}" ]; then
  echo "wasmtime_features: ${pin[WASMTIME_FEATURES]}"
else
  echo "wasmtime_features: not pinned yet (S0.2)"
fi

if [ -n "${pin[WIT_BINDGEN]:-}" ]; then
  lockfiles=(Cargo.lock)
  [ -f modules/Cargo.lock ] && lockfiles+=(modules/Cargo.lock)
  check_locked wit-bindgen "${pin[WIT_BINDGEN]}" "${lockfiles[@]}"
else
  echo "wit-bindgen: not pinned yet (S0.2)"
fi

if [ -n "${pin[WASM_TOOLS]:-}" ]; then
  want="${pin[WASM_TOOLS]}"
  if command -v wasm-tools >/dev/null 2>&1; then
    have="$(wasm-tools --version | awk '{print $2}')"
    if [ "$have" = "$want" ]; then
      echo "wasm-tools: $want (matches PATH)"
    else
      echo "wasm-tools: $want (FAIL: PATH has $have)"
      fail "wasm-tools pinned $want but PATH has $have"
    fi
  else
    echo "wasm-tools: $want (FAIL: not on PATH)"
    fail "wasm-tools pinned $want but not found on PATH"
  fi
else
  echo "wasm-tools: not pinned yet (S0.2)"
fi

wit_files=()
if [ -d modules/wit ]; then
  while IFS= read -r -d '' f; do wit_files+=("$f"); done \
    < <(find modules/wit -type f -name '*.wit' -print0 | LC_ALL=C sort -z)
fi
if [ "${#wit_files[@]}" -eq 0 ]; then
  echo "wit: none yet"
else
  for f in "${wit_files[@]}"; do
    echo "wit: $(sha256sum "$f" | awk '{print $1}') $f"
  done
fi

if [ "$problems" -eq 0 ]; then
  echo "module-toolchain: OK"
else
  echo "module-toolchain: FAILED ($problems problems)"
  exit 1
fi
