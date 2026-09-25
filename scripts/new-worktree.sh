#!/usr/bin/env bash
# One worktree per task, so parallel agents never share a checkout. Each gets a cold target
# under ~/.cache/cargo-target on the SSD (ext4); jobs=2 and scripts/rustc-serial; see AGENTS.md.
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

cat <<EOF
worktree: $worktree
branch:   task/$slug

  cd $worktree
  scripts/gate.sh
EOF
