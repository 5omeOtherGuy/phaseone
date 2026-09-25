#!/usr/bin/env bash
# p1's build farm entry point: push a task branch, wait for the GitHub Actions run
# of exactly that commit, print the run summary and download the built binary.
#
#   scripts/ci-build.sh [<branch>] [--no-download] [--wait-only]
#
#   <branch>        branch to push and wait for (default: the current branch)
#   --no-download   print the run summary only, do not download the artifact
#   --wait-only     do not push; wait for the run of the branch's existing commit
#
# Exit codes:
#   0  the run concluded success and the downloaded p1 passed `sha256sum -c`
#   1  the run failed or was cancelled, or the downloaded p1 does not match the
#      uploaded p1.sha256 (the failed step's log tail is printed)
#   2  usage or tooling error: bad arguments, a detached HEAD, `main`, a failed
#      push, a missing local tool, a failed `gh` call, no run for that commit, no
#      completed run within CI_BUILD_TIMEOUT seconds, or a failed download:
#      nothing can be concluded about the commit
#
# The push must come from the branch's checkout, so a detached HEAD is refused
# whether or not a branch was named.
#
# Run identity: only runs of `.github/workflows/build.yml` for the exact headSha
# count, and only a run this invocation caused:
#
#   CI_TRIGGER=push      (default) event `push`. A push that moves the branch
#                        accepts only a run that did not exist before it; a push
#                        that moves nothing (the commit is already on the branch)
#                        waits for that commit's own newest run, because such a
#                        push creates no run at all.
#   CI_TRIGGER=dispatch  nothing runs on push: reuse a build.yml run for the commit
#                        that is queued, running or green, otherwise start one with
#                        `gh workflow run build.yml --ref <branch>` and accept only
#                        a run id that did not exist before the dispatch.
#
# Any other CI_TRIGGER value is a tooling error (exit 2).
#
# Environment: CI_TRIGGER (push|dispatch, default push), CI_BUILD_TIMEOUT (seconds
# to wait for the run, default 1800), CI_BUILD_INTERVAL (seconds between polls,
# default 20).
#
# It never force-pushes and pushes only <branch>.
set -euo pipefail
cd "$(dirname "$0")/.."

artifact=p1-build
workflow=build.yml

usage() {
  cat >&2 <<'EOF'
usage: scripts/ci-build.sh [<branch>] [--no-download] [--wait-only]
  <branch>        branch to push and wait for (default: the current branch)
  --no-download   print the run summary only; do not download the artifact
  --wait-only     do not push; wait for the run of the branch's existing commit
env: CI_TRIGGER=push|dispatch (default push; dispatch reuses or starts a build.yml run)
exit: 0 success, 1 run failed or cancelled, 2 usage or tooling error
EOF
}

# A missing tool must be a tooling error (2), not whatever `set -e` would report.
for tool in git gh mktemp mkdir find mv sha256sum cut sleep; do
  command -v "$tool" >/dev/null 2>&1 ||
    { echo "ci-build: $tool is not on PATH" >&2; exit 2; }
done

trigger=${CI_TRIGGER:-push}
case "$trigger" in
  push|dispatch) ;;
  *) echo "ci-build: CI_TRIGGER must be push or dispatch, not '$trigger'" >&2; exit 2 ;;
esac

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

current=$(git rev-parse --abbrev-ref HEAD 2>/dev/null) ||
  { echo "ci-build: not in a git checkout; name a task branch" >&2; exit 2; }
if [ "$current" = HEAD ]; then
  echo "ci-build: refusing a detached HEAD; the push must come from the branch's checkout" >&2
  exit 2
fi
if [ -z "$branch" ]; then
  branch=$current
fi
case "$branch" in
  main|refs/heads/main|origin/main)
    echo "ci-build: refusing to push $branch; use scripts/push-main.sh for main" >&2
    exit 2
    ;;
esac
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

# Ids of the build.yml runs of this commit that already exist, space separated:
# a run this invocation causes cannot be one of them.
matching_ids() {
  local ids
  ids=$(gh run list --workflow "$workflow" --limit 50 --json databaseId,headSha \
          -q ".[] | select(.headSha==\"$sha\") | .databaseId") || return 1
  printf '%s' "${ids//$'\n'/ }"
}

# The run to wait for, one TSV line "status<TAB>conclusion<TAB>id" per candidate,
# newest first. Once a run is picked, `pinned` keeps the loop on that run.
list_run() {
  local fields="status,conclusion,headSha,event,databaseId"
  local excl="" filter id
  if [ "$require_new" = 1 ] && [ -n "$snapshot" ]; then
    local -a ids
    read -r -a ids <<<"$snapshot"
    for id in "${ids[@]}"; do
      if [ -n "$id" ]; then
        excl="$excl and .databaseId != $id"
      fi
    done
  fi
  if [ -n "$pinned" ]; then
    filter=".[] | select(.databaseId==$pinned) | [.status, .conclusion, .databaseId] | @tsv"
  elif [ "$trigger" = push ]; then
    # Only build.yml's push run of this exact commit.
    filter=".[] | select(.headSha==\"$sha\" and .event==\"push\"$excl) | [.status, .conclusion, .databaseId] | @tsv"
  elif [ "$require_new" = 1 ]; then
    # The dispatch this invocation started, not a run that already existed.
    filter=".[] | select(.headSha==\"$sha\" and .event==\"workflow_dispatch\"$excl) | [.status, .conclusion, .databaseId] | @tsv"
  else
    # A build.yml run of this commit that is still queued or running, or already green.
    filter=".[] | select(.headSha==\"$sha\") | select((.status != \"completed\") or (.conclusion == \"success\")) | [.status, .conclusion, .databaseId] | @tsv"
  fi
  if [ "$trigger" = push ]; then
    gh run list --branch "$branch" --workflow "$workflow" --limit 30 --json "$fields" -q "$filter"
  else
    gh run list --workflow "$workflow" --limit 30 --json "$fields" -q "$filter"
  fi
}

snapshot=""
pinned=""
push_moved=0
require_new=0
if [ "$push" = 1 ]; then
  remote_line=$(git ls-remote --heads origin "refs/heads/$branch" 2>/dev/null) ||
    { echo "ci-build: git ls-remote origin $branch failed" >&2; exit 2; }
  remote_sha=""
  if [ -n "$remote_line" ]; then
    remote_sha=${remote_line%%$'\t'*}
  fi
  snapshot=$(matching_ids) || { echo "ci-build: gh run list failed" >&2; exit 2; }
  if ! git push --quiet origin "refs/heads/$branch:refs/heads/$branch"; then
    echo "ci-build: git push origin $branch failed" >&2
    exit 2
  fi
  if [ "$remote_sha" != "$sha" ]; then
    # The push moved the branch, so a run that already existed is not this one's.
    push_moved=1
  fi
  echo "pushed $branch ${sha:0:7}; waiting for its build run…"
else
  # --wait-only: wait for the run of the commit the branch already has.
  snapshot=$(matching_ids) || { echo "ci-build: gh run list failed" >&2; exit 2; }
  echo "wait-only: not pushing; waiting for the run of $branch ${sha:0:7}…"
fi
if [ "$trigger" = push ]; then
  require_new=$push_moved
fi

if [ "$trigger" = dispatch ]; then
  # Reuse a run that already exists (queued, running or green); --wait-only never
  # starts one.
  existing=$(list_run) || { echo "ci-build: gh run list failed" >&2; exit 2; }
  if [ -n "$existing" ]; then
    echo "dispatch: reusing the build.yml run already queued, running or green for ${sha:0:7}"
  elif [ "$push" = 1 ]; then
    if ! gh workflow run "$workflow" --ref "$branch"; then
      echo "ci-build: gh workflow run $workflow --ref $branch failed" >&2
      exit 2
    fi
    require_new=1
    echo "dispatch: started $workflow on $branch for ${sha:0:7}"
  else
    echo "wait-only: no build.yml run for ${sha:0:7} yet; waiting…"
  fi
fi

SECONDS=0
while :; do
  runs=$(list_run) || { echo "ci-build: gh run list failed" >&2; exit 2; }
  run_line=${runs%%$'\n'*}
  status=""
  conclusion=""
  run_id=""
  if [ -n "$run_line" ]; then
    IFS=$'\t' read -r status conclusion run_id <<<"$run_line" || true
  fi
  if [ -n "$run_id" ]; then
    pinned=$run_id
  fi
  if [ "$status" = completed ]; then
    break
  fi
  echo "  ${status:-no run for ${sha:0:7} yet}"
  if [ "$SECONDS" -ge "$timeout_s" ]; then
    echo "ci-build: no completed run for ${sha:0:7} on $branch within ${timeout_s}s (last status: ${status:-none})" >&2
    exit 2
  fi
  if ! sleep "$interval_s"; then
    echo "ci-build: sleep failed" >&2
    exit 2
  fi
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
if ! tmpdir=$(mktemp -d); then
  echo "ci-build: mktemp -d failed" >&2
  exit 2
fi
trap 'rm -rf -- "$tmpdir"' EXIT
if ! gh run download "$run_id" --name "$artifact" --dir "$tmpdir"; then
  echo "ci-build: could not download the $artifact artifact of run $run_id" >&2
  exit 2
fi
if ! mkdir -p "$dest"; then
  echo "ci-build: cannot create $dest" >&2
  exit 2
fi
if ! find "$tmpdir" -type f -exec mv -t "$dest" {} +; then
  echo "ci-build: cannot move the artifact files into $dest" >&2
  exit 2
fi
for file in p1 p1.sha256 gate.log; do
  if [ ! -f "$dest/$file" ]; then
    echo "ci-build: the $artifact artifact has no $file" >&2
    exit 2
  fi
done
if ! local_sha=$(sha256sum "$dest/p1"); then
  echo "ci-build: sha256sum failed on $dest/p1" >&2
  exit 2
fi
local_sha=${local_sha%% *}
if ! uploaded_sha=$(cut -d' ' -f1 "$dest/p1.sha256"); then
  echo "ci-build: cannot read $dest/p1.sha256" >&2
  exit 2
fi
echo "sha256 (downloaded) $local_sha"
echo "sha256 (uploaded)   $uploaded_sha"
if ! ( cd "$dest" && sha256sum -c p1.sha256 ); then
  echo "ci-build: sha256 verification failed for $dest/p1" >&2
  exit 1
fi
echo "ci-build: $branch ${sha:0:7} green; $dest/p1 verified with sha256sum -c"
exit 0
