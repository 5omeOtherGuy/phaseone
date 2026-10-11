#!/usr/bin/env bash
# One worktree per task, so parallel agents never share a checkout. Each gets a target of its own
# under ~/.cache/cargo-target on the SSD (ext4); jobs=3 and scripts/rustc-serial; see AGENTS.md.
# The target starts warm: hardlinks of the dependency template scripts/target-template.sh makes,
# so dependencies cost no disk and stay fresh, and cargo compiles only p1's own crates.
#
#   scripts/new-worktree.sh 12-jsonl-journal        # -> ../phaseone-12-jsonl-journal
#
# Creates the worktree on a fresh branch task/<slug> based on the latest main.
set -euo pipefail

slug="${1:?usage: scripts/new-worktree.sh <task-slug> [base-branch]}"
base="${2:-main}"

root="$(cd "$(dirname "$0")/.." && pwd)"
worktree="$(dirname "$root")/$(basename "$root")-$slug"

if [ -e "$worktree" ]; then
  echo "worktree already exists: $worktree" >&2
  exit 1
fi

git -C "$root" fetch --quiet origin "$base" 2>/dev/null || true
if git -C "$root" rev-parse --verify --quiet "origin/$base" >/dev/null; then
  start="origin/$base"
else
  start="$base"
fi

git -C "$root" worktree add "$worktree" -b "task/$slug" "$start"
"$root/scripts/local-cargo-config.sh" "$worktree"

template="$HOME/.cache/cargo-target/p1-template"
target="$(sed -n 's/^target-dir *= *"\(.*\)"$/\1/p' "$worktree/.cargo/config.toml")"
if [ ! -d "$template" ]; then
  echo "no target template; the target starts cold (scripts/target-template.sh makes one)" >&2
elif [ -n "$(ls -A "$target")" ]; then
  echo "target $target is not empty; not seeded" >&2
elif cp -al "$template/." "$target/" 2>/dev/null; then
  echo "seeded $target from $template"
else
  find "$target" -mindepth 1 -delete
  echo "could not hardlink $template into $target; the target starts cold" >&2
fi

cat <<EOF
worktree: $worktree
branch:   task/$slug

  cd $worktree
  scripts/gate.sh
EOF
