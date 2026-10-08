#!/usr/bin/env bash
# Retire a task worktree once its pull request merged (ADR-0128): refuses a dirty tree or a head
# that is neither on origin/main nor the exact head of a merged pull request, then removes the
# worktree, its local branch and its cargo target.
#   scripts/retire-worktree.sh <worktree-path>
set -euo pipefail
die() { echo "retire-worktree: $*" >&2; exit 1; }
wt="${1:?usage: scripts/retire-worktree.sh <worktree-path>}"
here="$(cd "$(dirname "$0")/.." && pwd)"
root="$(git -C "$here" worktree list --porcelain | sed -n '1s/^worktree //p')"
wt="$(realpath -- "$wt")" || die "no such path: $1"
[ "$wt" != "$root" ] || die "refusing to retire the main checkout $root"
git -C "$root" worktree list --porcelain | grep -qx "worktree $wt" || die "$wt is not a worktree of $root"
dirty="$(git -C "$wt" status --short --untracked-files=all | grep -v '^?? \.cargo/config\.toml$' || true)"
[ -z "$dirty" ] || die "$wt is not clean; commit, stash or inventory first:"$'\n'"$dirty"
branch="$(git -C "$wt" symbolic-ref --short -q HEAD || true)"
head="$(git -C "$wt" rev-parse HEAD)"
git -C "$root" fetch -q origin main || true
if ! git -C "$root" merge-base --is-ancestor "$head" origin/main; then
  # A squash merge leaves the branch head off main: accept it only as the exact head of a
  # merged pull request, so unpushed commits after the merge are never deleted.
  merged="$(cd "$root" && gh pr list --state merged --limit 200 --json headRefOid --jq '.[].headRefOid' 2>/dev/null || true)"
  grep -qx "$head" <<<"$merged" || die "head $head${branch:+ of $branch} is neither on origin/main nor the head of a merged pull request"
fi
target="$(sed -n 's/^target-dir *= *"\(.*\)"$/\1/p' "$wt/.cargo/config.toml" 2>/dev/null || true)"
git -C "$root" worktree remove --force "$wt"
[ -z "$branch" ] || git -C "$root" branch -D "$branch" >/dev/null
case "$target" in
  "$HOME/.cache/cargo-target/"?*|/data/build/?*) rm -rf -- "$target"; echo "removed target $target" ;;
  "") ;;
  *) echo "left target $target (outside the known build roots)" ;;
esac
echo "retired $wt${branch:+ (branch $branch)}"
