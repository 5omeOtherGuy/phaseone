#!/usr/bin/env bash
# The warm target template every new worktree's target starts from: the third-party dependencies
# of a warm target with p1's own crates dropped, so a new worktree compiles only p1.
#
#   scripts/target-template.sh [warm-target]    # refresh; default: the main checkout's target
#   scripts/target-template.sh --seed <target>  # copy the template into an empty target
#
# Refresh after a warm target built a changed Cargo.lock. The template lives in
# ~/.cache/cargo-target/p1-template.
set -euo pipefail
die() { echo "target-template: $*" >&2; exit 1; }

template="$HOME/.cache/cargo-target/p1-template"

# unshare_metadata <from> <to>: after `cp -al <from> <to>`, <to> shares only its compiled
# outputs, which rustc and the linker replace by rename. Cargo rewrites fingerprints, dep-info and
# build-script output in place, so through a shared inode a rebuild in one target would make a
# stale unit look fresh in the other (D20); those files become real copies (about 32 MiB).
unshare_metadata() {
  (cd "$2" && find . -type f ! \( -name '*.rlib' -o -name '*.rmeta' -o -name '*.so' -o -name '*.a' \
      -o -name '*.wasm' -o \( -perm -u+x ! -path '*/out/*' \) \) -print0) > "$2.metadata"
  (cd "$2" && xargs -0 -r rm -f -- < "$2.metadata")
  (cd "$1" && xargs -0 -r cp -p --parents -t "$2" -- < "$2.metadata")
  rm -f -- "$2.metadata"
}

if [ "${1-}" = --seed ]; then
  target="${2:?usage: scripts/target-template.sh --seed <target>}"
  [ -d "$template" ] || die "no template; scripts/target-template.sh makes one"
  [ -d "$target" ] && [ -z "$(ls -A "$target")" ] || die "$target is not an empty directory"
  if ! cp -al "$template/." "$target/"; then
    find "$target" -mindepth 1 -delete
    die "cannot hardlink $template into $target (another filesystem?)"
  fi
  unshare_metadata "$template" "$target"
  echo "seeded $target from $template"
  exit 0
fi

here="$(cd "$(dirname "$0")/.." && pwd)"
main="$(git -C "$here" worktree list --porcelain | sed -n '1s/^worktree //p')"
seed="${1:-$(sed -n 's/^target-dir *= *"\(.*\)"$/\1/p' "$main/.cargo/config.toml" 2>/dev/null || true)}"
[ -n "$seed" ] && [ -d "$seed" ] || die "no warm target${seed:+ at $seed}"
next="$template.new"

rm -rf -- "$next"
mkdir -p -- "$next"
cp -al "$seed/." "$next/" || die "cannot hardlink $seed into $next (another filesystem?)"
# Drop every unit of a p1-* package (all of p1's own crates, in both workspaces): cargo hashes
# them by their path relative to the workspace, so another checkout would take them for its own
# and link code built from different sources with this checkout's absolute paths (D20).
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
unshare_metadata "$seed" "$next"

rm -rf -- "$template"
mv -- "$next" "$template"
echo "template $template from $seed ($(du -s --apparent-size -h "$template" | cut -f1) of dependencies)"
