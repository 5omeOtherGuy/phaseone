#!/usr/bin/env bash
# Open the usage ledger as a live tmux window, so the owner can watch it outside a p1
# session. Idempotent: a window with that name is selected, never duplicated.
#
#   scripts/usage-pane.sh [--replace] [SECONDS] [WINDOW_NAME]   (defaults: 300, usage)
#
# The window runs `p1 usage --watch SECONDS --grid GRID` (GRID defaults to 48, the deployed
# pane width): the binary named by P1_BIN, else this checkout's own build, else the main
# checkout's build, else `cargo run`. Outside tmux the script starts a detached session when
# no server is running and prints the attach hint instead of taking over the terminal.
#
# `--replace` is the deployment path: the existing window's scrollback is captured to a file
# FIRST, then cleared and the window respawned. If the capture fails, the clear and respawn
# are aborted, so stale frames are never destroyed without a preserved copy.
set -euo pipefail

replace=0
if [ "${1:-}" = "--replace" ]; then
  replace=1
  shift
fi
seconds="${1:-300}"
window="${2:-usage}"
grid="${P1_GRID:-48}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ -n "${P1_BIN:-}" ]; then
  if [ ! -x "$P1_BIN" ]; then
    echo "usage-pane: P1_BIN is not executable: $P1_BIN" >&2
    exit 1
  fi
  binary="$P1_BIN"
elif [ -x "$repo/target/debug/p1" ]; then
  # This checkout's own build first: the main checkout's binary can predate a worktree's
  # usage changes (a stale binary is what made every API-key route read "no usage endpoint").
  binary="$repo/target/debug/p1"
elif [ -x "$repo/../phaseone-target/debug/p1" ]; then
  binary="$repo/../phaseone-target/debug/p1"
else
  binary=""
fi
if [ -n "$binary" ]; then
  echo "usage-pane: binary $binary"
  command="$(printf '%q' "$binary") usage --watch $seconds --grid $grid"
else
  command="cd $(printf '%q' "$repo") && cargo run -q -p p1-host -- usage --watch $seconds --grid $grid"
fi

# Windows are addressed by name, which is only unique per session; `-a` widens the search
# to every session for the outside-tmux case.
#
# Returns 0 when a window has EXACTLY this name, 1 when the listing succeeded without it,
# and 2 when tmux could not list windows at all. The whole listing is read before it is
# matched: letting `grep -q` close the pipe early can SIGPIPE tmux, and `set -o pipefail`
# then reported a present window as absent (a duplicate window was observed 2026-09-24).
window_exists() {
  local listed status line
  status=0
  listed="$(tmux list-windows "$@" -F '#{window_name}')" || status=$?
  if [ "$status" -ne 0 ]; then
    echo "usage-pane: tmux list-windows failed (status $status)" >&2
    return 2
  fi
  while IFS= read -r line; do
    if [ "$line" = "$window" ]; then
      return 0
    fi
  done <<< "$listed"
  return 1
}

# Capture a window's existing scrollback before it is cleared, so the stale frames that
# caused the owner's "missing endpoints" report are preserved on disk, never just dropped.
# Returns non-zero (and removes the empty file) when the capture fails.
capture_history() {
  local file
  file="${P1_HISTORY_DIR:-${TMPDIR:-/tmp}}/usage-pane-$window-$(date +%Y%m%d-%H%M%S).txt"
  mkdir -p "$(dirname "$file")" 2>/dev/null || true
  if tmux capture-pane -p -S - -t "$1" > "$file" 2>/dev/null; then
    echo "usage-pane: history captured to $file"
    return 0
  fi
  rm -f "$file"
  echo "usage-pane: could not capture history of '$1'; refusing to clear it" >&2
  return 1
}

# The deployment path: capture, clear, respawn. A failed capture aborts before the clear, so
# history is never destroyed without a preserved copy.
replace_window() {
  capture_history "$1" || return 1
  tmux clear-history -t "$1"
  tmux respawn-window -k -t "$1" "$command"
}

if [ -n "${TMUX:-}" ]; then
  status=0
  window_exists || status=$?
  if [ "$status" -eq 2 ]; then
    exit 1
  elif [ "$status" -eq 0 ]; then
    if [ "$replace" -eq 1 ]; then
      replace_window "$window"
      tmux select-window -t "$window"
      echo "usage-pane: replaced window '$window' (history captured first)"
    else
      tmux select-window -t "$window"
      echo "usage-pane: selected existing window '$window'"
    fi
  else
    tmux new-window -n "$window" "$command"
    echo "usage-pane: opened window '$window'"
  fi
  exit 0
fi

if ! tmux has-session 2>/dev/null; then
  tmux new-session -d -n "$window" "$command"
  echo "usage-pane: started a tmux session with window '$window'; attach with: tmux attach"
else
  status=0
  window_exists -a || status=$?
  if [ "$status" -eq 2 ]; then
    exit 1
  elif [ "$status" -eq 0 ]; then
    if [ "$replace" -eq 1 ]; then
      replace_window "$window"
      echo "usage-pane: replaced window '$window' (history captured first); attach with: tmux attach"
    else
      echo "usage-pane: window '$window' already exists; attach with: tmux attach"
    fi
  else
    tmux new-window -n "$window" "$command"
    echo "usage-pane: opened window '$window'; attach with: tmux attach"
  fi
fi
