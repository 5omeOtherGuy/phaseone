#!/usr/bin/env bash
# Refresh the warm target template that scripts/new-worktree.sh hardlinks into every new
# worktree's target: the third-party dependencies of a warm target with p1's own crates dropped,
# so a new worktree compiles only p1. Run it after a warm target built a changed Cargo.lock.
#
#   scripts/target-template.sh [warm-target]    # default: the main checkout's target
#
# The template lives in ~/.cache/cargo-target/p1-template and costs no disk beyond its
# directories while the warm target still holds the same files.
set -euo pipefail
die() { echo "target-template: $*" >&2; exit 1; }

here="$(cd "$(dirname "$0")/.." && pwd)"
main="$(git -C "$here" worktree list --porcelain | sed -n '1s/^worktree //p')"
seed="${1:-$(sed -n 's/^target-dir *= *"\(.*\)"$/\1/p' "$main/.cargo/config.toml" 2>/dev/null || true)}"
[ -n "$seed" ] && [ -d "$seed" ] || die "no warm target${seed:+ at $seed}"
template="$HOME/.cache/cargo-target/p1-template"
next="$template.new"

rm -rf -- "$next"
mkdir -p -- "$next"
cp -al "$seed/." "$next/" || die "cannot hardlink $seed into $next (another filesystem?)"
# Drop every unit of a p1-* package (all of p1's own crates, in both workspaces): cargo names
# them by their path relative to the workspace, and a rebuild writes some of their files in
# place, which through a shared inode would hand one checkout's build to another (D20).
# Their units are found by the metadata hash in their fingerprint directories, and by name
# where cargo adds no hash (the modules' .wasm outputs).
find "$next" -type d -path '*/.fingerprint/p1-*' -prune -printf '%f\n' \
  | sed -n 's/.*\(-[0-9a-f]\{16\}\)$/\1/p' | sort -u > "$next/.p1-units"
find "$next" -mindepth 1 -printf '%p\n' | grep -F -f "$next/.p1-units" \
  | xargs -r -d '\n' rm -rf --
find "$next" -depth -mindepth 1 \( -name 'p1_*' -o -name 'libp1_*' -o -name 'p1-*' \) \
  -exec rm -rf -- {} +
# Locks, rustc's cache, uplifted binaries and incremental state belong to the seed's builds.
find "$next" -maxdepth 1 -type f -delete
for profile in "$next"/*/ "$next"/*/*/; do
  [ -d "$profile/deps" ] || continue
  find "$profile" -maxdepth 1 -type f -delete
  rm -rf -- "$profile/incremental"
done
rm -rf -- "$next/tmp"

rm -rf -- "$template"
mv -- "$next" "$template"
echo "template $template from $seed ($(du -s --apparent-size -h "$template" | cut -f1) of dependencies)"
