#!/usr/bin/env bash
# The warm target template every new worktree's target starts from: the third-party dependencies
# of a warm target with p1's own crates dropped, so a new worktree compiles only p1.
#
#   scripts/target-template.sh [warm-target]      # refresh; default: the main checkout's target
#   scripts/target-template.sh --seed <target>    # copy the template into an empty target
#   scripts/target-template.sh --absorb <target>  # add a target's new dependency units
#
# scripts/retire-worktree.sh absorbs every target before it deletes it, so a changed Cargo.lock
# reaches the template once one worktree that built it retires. The template lives in
# ~/.cache/cargo-target/p1-template.
set -euo pipefail
die() { echo "target-template: $*" >&2; exit 1; }

template="$HOME/.cache/cargo-target/p1-template"

# Seeds copy the template under a shared lock; refresh and absorb change it under an exclusive one.
lock() { mkdir -p -- "$(dirname "$template")"; exec 9>"$template.lock"; flock "$1" 9; }

# hashes <dir> <glob>: the hashes (-<16 hex>) of the units whose fingerprint directory matches <glob>.
hashes() {
  find "$1" -type d -path "*/.fingerprint/$2" -prune -printf '%f\n' \
    | sed -n 's/.*\(-[0-9a-f]\{16\}\)$/\1/p' | sort -u
}

# drop <dir> <hash-file>: delete every file and directory of the units in <hash-file>.
drop() {
  [ -s "$2" ] || return 0  # grep -f of an empty file exits unread: find would die of SIGPIPE
  find "$1" -mindepth 1 -printf '%p\n' | { grep -F -f "$2" || true; } | xargs -r -d '\n' rm -rf --
}

# unshare_metadata <from> <to>: after `cp -al <from> <to>`, <to> shares only its compiled
# outputs, which rustc and the linker replace by rename. Cargo rewrites fingerprints and dep-info
# in place and build scripts rewrite everything under out/ (wit-bindgen's .a files included), so
# through a shared inode a rebuild in one target would make a stale unit look fresh in the other
# (D20); those files become real copies (about 32 MiB).
unshare_metadata() {
  (cd "$2" && find . -type f \( -path '*/out/*' -o ! \( -name '*.rlib' -o -name '*.rmeta' \
      -o -name '*.so' -o -name '*.a' -o -name '*.wasm' -o -perm -u+x \) \) -print0) > "$2.metadata"
  (cd "$2" && xargs -0 -r rm -f -- < "$2.metadata")
  (cd "$1" && xargs -0 -r cp -p --parents -t "$2" -- < "$2.metadata")
  rm -f -- "$2.metadata"
}

if [ "${1-}" = --seed ]; then
  target="${2:?usage: scripts/target-template.sh --seed <target>}"
  [ -d "$template" ] || die "no template; scripts/target-template.sh makes one"
  [ -d "$target" ] && [ -z "$(ls -A "$target")" ] || die "$target is not an empty directory"
  lock -s
  if cp -al "$template/." "$target/" 2>/dev/null; then
    unshare_metadata "$template" "$target"
    echo "seeded $target from $template"
    exit 0
  fi
  # A target on the HDD build root cannot hardlink the SSD template: copy it (1.7 GiB).
  find "$target" -mindepth 1 -delete
  cp -a "$template/." "$target/" || { find "$target" -mindepth 1 -delete; die "cannot copy $template into $target"; }
  echo "seeded $target from $template (copied, another filesystem)"
  exit 0
fi

if [ "${1-}" = --absorb ]; then
  from="${2:?usage: scripts/target-template.sh --absorb <target>}"
  [ -d "$template" ] || die "no template; scripts/target-template.sh makes one"
  [ -d "$from" ] || die "no target $from"
  lock -x
  # Whole units only, never p1's (D20): a unit the template already holds keeps the template's
  # files. Real copies, so nothing stays shared with <target>.
  { hashes "$template" '*'; hashes "$from" 'p1-*'; } | sort -u > "$template.known"
  hashes "$from" '*' | comm -23 - "$template.known" > "$template.units"
  # grep with no pattern exits unread, and find dies of SIGPIPE: copy only when there are units.
  if [ -s "$template.units" ]; then
    (cd "$from" && find . -regextype posix-extended -regex '.*-[0-9a-f]{16}[^/]*' -prune -print \
      | { grep -F -f "$template.units" || true; } \
      | xargs -r -d '\n' cp -a --parents -t "$template")
  fi
  # A unit of another compiler never turns fresh again: keep only the units of the compiler that
  # built the newest one, so a toolchain update replaces the template instead of doubling it.
  newest="$(find "$template" -path '*/.fingerprint/*' -name '*.json' -printf '%T@ %p\n' \
    | sort -n | tail -n 1 | cut -d' ' -f2-)"
  rustc="$(grep -o '"rustc":[0-9]*' "$newest")"
  find "$template" -path '*/.fingerprint/*' -name '*.json' -print0 \
    | { xargs -0 grep -L -F "$rustc" || true; } \
    | sed -n 's|.*/\.fingerprint/[^/]*\(-[0-9a-f]\{16\}\)/[^/]*$|\1|p' | sort -u > "$template.stale"
  drop "$template" "$template.stale"
  echo "absorbed $(wc -l < "$template.units") units of $from into $template, dropped $(wc -l < "$template.stale") of an older compiler"
  rm -f -- "$template.known" "$template.units" "$template.stale"
  exit 0
fi

here="$(cd "$(dirname "$0")/.." && pwd)"
main="$(git -C "$here" worktree list --porcelain | sed -n '1s/^worktree //p')"
seed="${1:-$(sed -n 's/^target-dir *= *"\(.*\)"$/\1/p' "$main/.cargo/config.toml" 2>/dev/null || true)}"
[ -n "$seed" ] && [ -d "$seed" ] || die "no warm target${seed:+ at $seed}"
next="$template.new"

lock -x
rm -rf -- "$next"
mkdir -p -- "$next"
cp -al "$seed/." "$next/" || die "cannot hardlink $seed into $next (another filesystem?)"
# Drop every unit of a p1-* package (all of p1's own crates, in both workspaces): cargo hashes
# them by their path relative to the workspace, so another checkout would take them for its own
# and link code built from different sources with this checkout's absolute paths (D20).
# Their units are found by the metadata hash in their fingerprint directories, and by name
# where cargo adds no hash (the modules' .wasm outputs).
hashes "$next" 'p1-*' > "$next/.p1-units"
drop "$next" "$next/.p1-units"
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
