#!/usr/bin/env bash
# One worktree per task, so parallel agents never share a checkout or a target dir.
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

cat <<EOF
worktree: $worktree
branch:   task/$slug

  cd $worktree
  export CARGO_BUILD_JOBS=2
  scripts/gate.sh
EOF
