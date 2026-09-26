#!/usr/bin/env bash
# Reports whether scripts/build-modules.sh builds a package reproducibly: two throwaway
# checkouts of HEAD at different absolute paths each build it with their own CARGO_TARGET_DIR,
# and the two <package>.sha256 digests must be equal (frozen case S7-N7, the evidence of
# S7.5.3). It is deliberately not part of the gate: two cold guest builds would double the
# gate's module build.
#
# usage: scripts/check-module-reproducibility.sh [--package <name>]
#        scripts/check-module-reproducibility.sh --help
#
# --package <name>  build modules/<name>/ only (default p1-module-fixture)
# Exit 0 when the two digests are equal, 1 when they differ (both are printed), 2 on a usage or
# tool error. Both worktrees and their targets are removed on every exit path.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
usage: scripts/check-module-reproducibility.sh [--package <name>]
       scripts/check-module-reproducibility.sh --help

--package <name>  build only the package modules/<name>/ (default p1-module-fixture)
Exit 0 when two checkouts of HEAD give equal digests, 1 when they differ, 2 on a tool error.
EOF
}

die() {
  echo "module-reproducibility: $*" >&2
  exit 2
}

package="p1-module-fixture"
case "$#" in
  0) ;;
  1)
    case "$1" in
      --help | -h)
        usage
        exit 0
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    ;;
  2)
    if [ "$1" = --package ] && [ -n "$2" ]; then
      package="$2"
    else
      usage >&2
      exit 2
    fi
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

# The two checkouts differ in absolute path, and so do their targets: the remaps
# scripts/build-modules.sh sets must make the digests equal anyway.
scratch="$(mktemp -d "${TMPDIR:-/tmp}/p1-module-repro.XXXXXX")" ||
  die "cannot create a temporary directory"
first="$scratch/checkout-one"
second="$scratch/a-second-checkout-path"
target_first="$scratch/target-one"
target_second="$scratch/target-second"
worktrees=()

cleanup() {
  local wt
  if [ "${#worktrees[@]}" -gt 0 ]; then
    for wt in "${worktrees[@]}"; do
      git worktree remove --force -- "$wt" >/dev/null 2>&1 || true
    done
  fi
  rm -rf -- "$scratch"
}
trap cleanup EXIT

git worktree add --detach "$first" HEAD >/dev/null || die "cannot create the first worktree at $first"
worktrees+=("$first")
git worktree add --detach "$second" HEAD >/dev/null || die "cannot create the second worktree at $second"
worktrees+=("$second")

# Each checkout builds with its own target, so the two builds share no artifact. CARGO_TARGET_DIR
# is set for this throwaway build only: the frozen check needs the target path to differ between
# the two checkouts, and it lies outside either root, so the target-dir remap applies.
build_checkout() {  # $1 = worktree, $2 = target dir
  if ! (cd "$1" && CARGO_TARGET_DIR="$2" scripts/build-modules.sh --package "$package"); then
    die "the build of $package in $1 failed"
  fi
}

digest_of() {  # $1 = worktree
  local file="$1/modules/target/p1-modules/$package/$package.sha256" digest
  [ -f "$file" ] || die "no digest at $file"
  digest="$(awk 'NR == 1 { print $1 }' "$file")"
  [ -n "$digest" ] || die "$file names no digest"
  printf '%s' "$digest"
}

build_checkout "$first" "$target_first"
build_checkout "$second" "$target_second"

digest_first="$(digest_of "$first")"
digest_second="$(digest_of "$second")"

if [ "$digest_first" = "$digest_second" ]; then
  echo "module-reproducibility: $package sha256:$digest_first equal across $first and $second"
  exit 0
fi
echo "module-reproducibility: $package sha256:$digest_first at $first differs from sha256:$digest_second at $second"
exit 1
