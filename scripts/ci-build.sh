#!/usr/bin/env bash
# p1's build farm entry point: push a task branch, wait for the GitHub Actions run
# of exactly that commit (as scripts/push-main.sh does for main), print the run
# summary and download the built binary.
#
#   scripts/ci-build.sh [<branch>] [--no-download] [--wait-only]
#
#   <branch>        branch to push and wait for (default: the current branch)
#   --no-download   print the run summary only, do not download the artifact
#   --wait-only     do not push; wait for the run of the branch's existing commit
#
# Exit codes:
#   0  the run concluded success and the downloaded p1 matches its uploaded sha256
#   1  the run failed, was cancelled or timed out, or the artifact's sha256 does
#      not match the uploaded one (the failed step's log tail is printed)
#   2  usage or tooling error: bad arguments, `main`, a failed push, a failed
#      `gh` call, no run for that commit, no completed run within CI_BUILD_TIMEOUT
#      seconds, or a failed download: nothing can be concluded about the commit
#
# Environment: CI_BUILD_TIMEOUT (seconds to wait for the run, default 1800),
# CI_BUILD_INTERVAL (seconds between polls, default 20).
#
# It never force-pushes and pushes only <branch>.
set -euo pipefail
cd "$(dirname "$0")/.."

artifact=p1-build

usage() {
  cat >&2 <<'EOF'
usage: scripts/ci-build.sh [<branch>] [--no-download] [--wait-only]
  <branch>        branch to push and wait for (default: the current branch)
  --no-download   print the run summary only; do not download the artifact
  --wait-only     do not push; wait for the run of the branch's existing commit
exit: 0 success, 1 run failed or cancelled, 2 usage or tooling error
EOF
}

command -v git >/dev/null 2>&1 || { echo "ci-build: git is not on PATH" >&2; exit 2; }
command -v gh >/dev/null 2>&1 || { echo "ci-build: gh is not on PATH" >&2; exit 2; }

branch=""
download=1
push=1
while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-download) download=0 ;;
    --wait-only) push=0 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "ci-build: unknown option: $1" >&2; usage; exit 2 ;;
    *)
      if [ -n "$branch" ]; then
        echo "ci-build: more than one branch given: $branch and $1" >&2
        usage
        exit 2
      fi
      branch="$1"
      ;;
  esac
  shift
done

if [ -z "$branch" ]; then
  branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null) ||
    { echo "ci-build: not in a git checkout; name a task branch" >&2; exit 2; }
fi
case "$branch" in
  main|refs/heads/main|origin/main)
    echo "ci-build: refusing to push $branch; use scripts/push-main.sh for main" >&2
    exit 2
    ;;
esac
if [ "$branch" = HEAD ]; then
  echo "ci-build: detached HEAD; name a task branch" >&2
  exit 2
fi
sha=$(git rev-parse --verify --quiet "refs/heads/$branch" 2>/dev/null) ||
  { echo "ci-build: no local branch $branch" >&2; exit 2; }

timeout_s=${CI_BUILD_TIMEOUT:-1800}
interval_s=${CI_BUILD_INTERVAL:-20}
for value in "$timeout_s" "$interval_s"; do
  case "$value" in
    ''|*[!0-9]*)
      echo "ci-build: CI_BUILD_TIMEOUT and CI_BUILD_INTERVAL take whole seconds" >&2
      exit 2
      ;;
  esac
done

if [ "$push" = 1 ]; then
  if ! git push --quiet origin "refs/heads/$branch:refs/heads/$branch"; then
    echo "ci-build: git push origin $branch failed" >&2
    exit 2
  fi
  echo "pushed $branch ${sha:0:7}; waiting for its build run…"
else
  echo "wait-only: not pushing; waiting for the run of $branch ${sha:0:7}…"
fi

deadline=$(( $(date +%s) + timeout_s ))
while :; do
  # Exactly this commit's run, as scripts/push-main.sh does for main.
  runs=$(gh run list --branch "$branch" --limit 20 \
           --json status,conclusion,headSha,databaseId \
           -q ".[] | select(.headSha==\"$sha\") | [.status, .conclusion, .databaseId] | @tsv") ||
    { echo "ci-build: gh run list failed" >&2; exit 2; }
  run_line=${runs%%$'\n'*}
  status=""
  conclusion=""
  run_id=""
  if [ -n "$run_line" ]; then
    IFS=$'\t' read -r status conclusion run_id <<<"$run_line" || true
  fi
  if [ "$status" = completed ]; then
    break
  fi
  echo "  ${status:-no run for ${sha:0:7} yet}"
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "ci-build: no completed run for ${sha:0:7} on $branch within ${timeout_s}s (last status: ${status:-none})" >&2
    exit 2
  fi
  sleep "$interval_s"
done

if ! summary=$(gh run view "$run_id" \
      --json displayTitle,status,conclusion,url \
      -q '"\(.displayTitle) [\(.status)/\(.conclusion)] \(.url)"'); then
  echo "ci-build: gh run view $run_id failed" >&2
  exit 2
fi
echo "run $run_id: $summary"

if [ "$conclusion" != success ]; then
  echo "ci-build: run $run_id concluded '${conclusion:-unknown}' for ${sha:0:7} on $branch; failed step log tail:" >&2
  if failed_log=$(gh run view "$run_id" --log-failed 2>/dev/null); then
    printf '%s\n' "$failed_log" | tail -n 40 >&2
  else
    echo "ci-build: no failed-step log for run $run_id" >&2
  fi
  exit 1
fi

if [ "$download" = 0 ]; then
  echo "ci-build: $branch ${sha:0:7} green (artifact not downloaded)"
  exit 0
fi

dest="ci-artifacts/$sha"
tmpdir=$(mktemp -d)
trap 'rm -rf -- "$tmpdir"' EXIT
if ! gh run download "$run_id" --name "$artifact" --dir "$tmpdir"; then
  echo "ci-build: could not download the $artifact artifact of run $run_id" >&2
  exit 2
fi
mkdir -p "$dest"
find "$tmpdir" -type f -exec mv -t "$dest" {} +
if [ ! -f "$dest/p1" ] || [ ! -f "$dest/p1.sha256" ]; then
  echo "ci-build: the $artifact artifact has no p1 and p1.sha256" >&2
  exit 2
fi
local_sha=$(sha256sum "$dest/p1")
local_sha=${local_sha%% *}
uploaded_sha=$(cut -d' ' -f1 "$dest/p1.sha256")
echo "sha256 (downloaded) $local_sha"
echo "sha256 (uploaded)   $uploaded_sha"
if [ "$local_sha" != "$uploaded_sha" ]; then
  echo "ci-build: sha256 mismatch for $dest/p1" >&2
  exit 1
fi
echo "ci-build: $branch ${sha:0:7} green; $dest/p1 verified against p1.sha256"
exit 0
