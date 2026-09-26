#!/usr/bin/env bash
# Records what the module runtime costs on this box (S0.8): the cold and warm readings of
# `crates/p1-module-tests/src/bin/bench-baseline.rs`, the box's max RSS for them, the sizes of
# the binaries involved, and the storage of the two build trees — each bound to the commit,
# the compiler and the module toolchain pins it was recorded with.
#
# Facts, not a check: the record carries no target and no threshold, and the gate does not run
# this suite. One box and one commit say what the runtime costs there; the programme lead
# compares the records.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
usage: scripts/bench-modules.sh --suite baseline [--out <path>]
       scripts/bench-modules.sh --help

--suite <name>  the suite to record; baseline is the only one
--out <path>    where the record goes (default .worker-scratch/bench-baseline-<shortsha>.txt)

The baseline suite builds missing module packages, builds the bench binary in the debug
profile the gate uses, runs it under /usr/bin/time -v, and writes one record of wall time,
max RSS, binary size and build storage, with the commit, rustc -V and the module toolchain
pins. The record is printed and its path is the last line.
Exit 0 when the record is written, 1 when a fact could not be measured, 2 on a usage error.
EOF
}

suite=""
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --suite)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      suite="$2"
      shift 2
      ;;
    --out)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      out="$2"
      shift 2
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

if [ -z "$suite" ]; then
  usage >&2
  exit 2
fi
if [ "$suite" != baseline ]; then
  echo "bench-modules: suite $suite is not baseline, the only suite" >&2
  exit 2
fi

fail() {
  echo "bench-modules: $*" >&2
  exit 1
}

[ -x /usr/bin/time ] || fail "/usr/bin/time is missing, so the bench's max RSS cannot be measured"

package=p1-module-fixture
package_dir="modules/target/p1-modules/$package"
fixture_wasm="$package_dir/$package.wasm"

# The fixture is the module every reading is about; the build publishes it, so a missing one
# is built rather than recorded as an absent number.
if [ ! -f "$fixture_wasm" ] || [ ! -f "$package_dir/$package.manifest.json" ]; then
  echo "bench-modules: the fixture package is not built; running scripts/build-modules.sh --all" >&2
  scripts/build-modules.sh --all >&2
fi

# The gate builds through this checkout's machine-local target (scripts/gate.sh does the
# same); a record taken against another target directory would describe a build the gate
# never makes.
if [ -z "${CI:-}" ] && [ ! -f .cargo/config.toml ]; then
  scripts/local-cargo-config.sh >&2
fi

echo "bench-modules: building the bench binary in the debug profile" >&2
cargo build --locked -p p1-module-tests --bin bench-baseline >&2
# The record names the runtime's own rlib, which Cargo lifts into the target directory's
# debug/ only when the library is itself a target of the build.
cargo build --locked -p p1-module-runtime >&2

target_dir="$(cargo metadata --format-version 1 --no-deps --locked |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$target_dir" ] || fail "cannot read the target directory of this workspace"
bench_bin="$target_dir/debug/bench-baseline"
rlib="$target_dir/debug/libp1_module_runtime.rlib"
[ -x "$bench_bin" ] || fail "the bench binary is not at $bench_bin"

# The readings and /usr/bin/time's report are two answers from one process: the bench's own
# output on stdout, the report on stderr.
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
bench_out="$scratch/bench.txt"
time_report="$scratch/time.txt"
if ! /usr/bin/time -v "$bench_bin" >"$bench_out" 2>"$time_report"; then
  cat "$time_report" >&2
  fail "the bench binary failed at $bench_bin"
fi

max_rss_kb="$(awk -F': ' '/Maximum resident set size \(kbytes\)/ {print $2}' "$time_report")"
[ -n "$max_rss_kb" ] || fail "cannot read the max RSS from /usr/bin/time -v"

file_bytes() {
  local path="$1"
  [ -f "$path" ] || fail "missing $path"
  wc -c <"$path" | tr -d ' '
}

dir_bytes() {
  local path="$1"
  [ -d "$path" ] || fail "missing $path"
  du -sb "$path" | awk '{print $1}'
}

head_sha="$(git rev-parse HEAD)"
short_sha="$(git rev-parse --short HEAD)"
rustc_line="$(rustc -V)"
toolchain="$(scripts/module-toolchain.sh --check)" ||
  fail "scripts/module-toolchain.sh --check failed, so the record's toolchain is unverified"

record="${out:-.worker-scratch/bench-baseline-$short_sha.txt}"
mkdir -p "$(dirname "$record")" || fail "cannot create $(dirname "$record")"
{
  printf 'bench-modules: baseline record\n\n'
  printf 'suite: %s\n' "$suite"
  printf 'date -u: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'git HEAD: %s\n' "$head_sha"
  printf 'rustc -V: %s\n' "$rustc_line"
  printf '\n-- bench binary output (%s)\n' "$bench_bin"
  cat "$bench_out"
  printf '\n-- max RSS (/usr/bin/time -v of the bench binary)\n'
  printf 'max_rss: %s kB (%s MiB)\n' "$max_rss_kb" \
    "$(awk -v kb="$max_rss_kb" 'BEGIN { printf "%.1f", kb / 1024 }')"
  printf '\n-- binary sizes\n'
  printf 'fixture_wasm: %s bytes (%s)\n' "$(file_bytes "$fixture_wasm")" "$fixture_wasm"
  printf 'bench_binary: %s bytes (%s)\n' "$(file_bytes "$bench_bin")" "$bench_bin"
  printf 'libp1_module_runtime_rlib: %s bytes (%s)\n' "$(file_bytes "$rlib")" "$rlib"
  printf '\n-- build storage (du -sb)\n'
  printf 'modules_target: %s bytes (%s)\n' "$(dir_bytes modules/target)" "modules/target"
  printf 'cargo_target: %s bytes (%s)\n' "$(dir_bytes "$target_dir")" "$target_dir"
  printf '\n-- module toolchain (scripts/module-toolchain.sh --check)\n'
  printf '%s\n' "$toolchain"
} >"$record"

cat "$record"
printf 'bench-modules: baseline recorded (%s)\n' "$record"
