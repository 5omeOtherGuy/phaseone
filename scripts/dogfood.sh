#!/usr/bin/env bash
# One supervised dogfooding run of p1 on a DISPOSABLE CLONE of a repository.
#
#   scripts/dogfood.sh <label> <env> <repo> <task-file> [base-ref]
#
# - The clone lives in ../phaseone-dogfood/<label>; it is self-contained (a worktree's .git
#   is outside it, a clone's is not), so the shell sandbox can confine the agent to it.
# - The agent runs with --yes inside the workspace sandbox; cargo's registry and the rustc
#   semaphore directory stay writable so builds work and stay serialized machine-wide.
# - Afterwards: the session journal, the agent's output, the diff and one evidence record
#   (scripts/run-report.py) are in ../phaseone-dogfood/<label>.run/. NOTHING is merged and
#   acceptance is NOT decided here: verify independently, then append the record with
#   --accepted yes|no to docs/dogfood/runs.jsonl.
set -euo pipefail
label=$1 env=$2 repo=$(realpath "$3") task_file=$(realpath "$4") base=${5:-HEAD}
here=$(cd "$(dirname "$0")/.." && pwd)
root=$(realpath "$here/..")/phaseone-dogfood
clone=$root/$label run=$root/$label.run
[ -e "$clone" ] && { echo "exists: $clone — pick another label or remove it" >&2; exit 2; }
mkdir -p "$run"
git clone -q --local "$repo" "$clone"
git -C "$clone" checkout -q --detach "$base"
# A p1 clone builds into its OWN target/ (inside the sandbox's writable area), seeded with
# hardlinks from the shared target. Run THIS checkout's script: run from inside the clone it
# would take the clone for the main checkout and point the target dir outside of it.
if [ -f "$clone/scripts/local-cargo-config.sh" ]; then "$here/scripts/local-cargo-config.sh" "$clone" >/dev/null; fi
p1=${P1_BIN:-$here/../phaseone-target/debug/p1}
[ -x "$p1" ] || { echo "no p1 binary at $p1 — run: cargo build -p p1-host" >&2; exit 2; }
locks=/tmp/p1-build-locks-$(id -u); mkdir -p "$locks"
# A plain clone keeps its `.git` inside the workspace; a worktree keeps it in the main
# checkout, which the sandbox hides. Make that common dir readable (never writable) so the
# agent can run `git status`/`git diff`. Read-only on purpose: it may inspect, not commit.
common=$(git -C "$clone" rev-parse --git-common-dir)
case "$common" in
  /*) ;;
  *) common="$clone/$common" ;;
esac
common=$(realpath -m "$common")
read_args=()
if [ "$common" != "$clone" ] && [[ "$common" != "$clone"/* ]]; then
  read_args=(--sandbox-read "$common")
fi
start=$(date +%s)
set +e
"$p1" --env "$env" --workspace "$clone" --session "$run/session.jsonl" --yes \
  --sandbox workspace --sandbox-write "$HOME/.cargo/registry" --sandbox-write "$HOME/.cargo/git" \
  --sandbox-write "$locks" "${read_args[@]}" "$(cat "$task_file")" >"$run/stdout.txt" 2>"$run/stderr.txt"
code=$?
set -e
elapsed=$(( $(date +%s) - start ))
git -C "$clone" add -A -N . >/dev/null 2>&1 || true
git -C "$clone" diff >"$run/changes.diff" || true
cp "$task_file" "$run/task.txt"
"$here/scripts/run-report.py" "$run/session.jsonl" --label "$label" --elapsed "$elapsed" \
  --exit-code "$code" >"$run/report.json"
echo "exit=$code elapsed=${elapsed}s clone=$clone run=$run"
tail -n 3 "$run/stderr.txt" || true
