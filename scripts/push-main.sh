#!/usr/bin/env bash
# Push main, then WAIT for the CI run of exactly that commit and fail if it is not green.
# Exists because "CI green" was twice reported or assumed from an earlier run (review R7,
# and again after the sandbox merge). Use this instead of a bare `git push origin main`.
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "not on main" >&2; exit 2; }
git push -q origin main
sha=$(git rev-parse HEAD)
echo "pushed ${sha:0:7}; waiting for CI…"
for _ in $(seq 1 90); do
  run=$(gh run list --branch main --limit 10 --json status,conclusion,headSha \
        -q ".[] | select(.headSha==\"$sha\") | [.status, .conclusion] | @tsv" | head -1)
  case "$run" in
    completed*success*) echo "CI green on ${sha:0:7}"; exit 0 ;;
    completed*) echo "CI NOT green on ${sha:0:7}: $run" >&2; exit 1 ;;
  esac
  sleep 20
done
echo "CI did not finish in 30 min for ${sha:0:7}" >&2; exit 1
