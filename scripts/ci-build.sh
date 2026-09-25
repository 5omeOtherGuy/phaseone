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
#      push or a push whose status line cannot be read, a missing or failing local
#      tool, a failed `gh` call, no run for that commit, no completed run within
#      CI_BUILD_TIMEOUT seconds, or a failed download: nothing can be concluded
#      about the commit
#
# The push must come from the branch's checkout, so a detached HEAD is refused
# whether or not a branch was named.
#
# Download: the artifact tree is validated and staged with its relative paths into
# ci-artifacts/<sha>/ (p1, p1.sha256 and gate.log must sit at its root). Only
# regular files and directories are accepted, so a symlink, FIFO, socket or device
# is a tooling error; a fresh staging directory beside the destination is renamed
# into place, so a failure never leaves a half-written destination and an earlier
# download of the same commit is replaced cleanly.
#
# Run identity: a run is the answer when it is a run of `.github/workflows/build.yml`
# for this exact headSha, it is not one this invocation has already seen (the
# pre-action snapshot of run ids), and its event matches the action taken:
#
#   CI_TRIGGER=push      (default) event `push`. When this invocation's own push
#                        moved the branch (its porcelain status line is ` `, `+` or
#                        `*`), only a run that appeared after the snapshot is
#                        accepted. When the push changed nothing (`=` up to date:
#                        another caller already pushed this commit, so GitHub
#                        creates no run for us), that commit's newest push run is
#                        the answer, because the snapshot would exclude the only
#                        run there is.
#   CI_TRIGGER=dispatch  event `workflow_dispatch`. Reuse a build.yml run of the
#                        commit that is queued, running or green; otherwise start
#                        one with `gh workflow run build.yml --ref <branch>` and
#                        accept any run for the commit that appeared after the
#                        snapshot.
#
# Identity is the commit plus the workflow file, not the caller: a run a concurrent
# invocation started for the same commit and workflow is the same build, so it is
# accepted and there is no caller-correlation token.
#
# Any other CI_TRIGGER value is a tooling error (exit 2).
#
# Environment: CI_TRIGGER (push|dispatch, default push), CI_BUILD_TIMEOUT (seconds
# to wait for the run, default 1800), CI_BUILD_INTERVAL (seconds between polls,
# default 20).
#
# It never force-pushes and pushes only <branch>.
set -euo pipefail

# A missing tool must be a tooling error (2), not whatever `set -e` would report.
# This runs before the first external command (dirname, below) and must list every
# external command the script runs: git and gh, the artifact and log tooling, and
# the tool the exit trap uses.
for tool in git gh dirname cat mktemp mkdir find cp mv cut sha256sum tail rm sleep; do
  command -v "$tool" >/dev/null 2>&1 ||
    { echo "ci-build: $tool is not on PATH" >&2; exit 2; }
done

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
    # Any build.yml dispatch run of this commit that appeared after the snapshot,
    # this invocation's or a concurrent caller's (same commit, same workflow).
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
  snapshot=$(matching_ids) || { echo "ci-build: gh run list failed" >&2; exit 2; }
  refspec="refs/heads/$branch:refs/heads/$branch"
  # Whether THIS push moved the branch comes from the push's own machine-readable
  # status, not from a value read before it: a concurrent caller pushing the same
  # commit would otherwise make our no-op push look like the one that moved it.
  push_out=$(git push --porcelain origin "$refspec") ||
    { echo "ci-build: git push origin $branch failed" >&2; exit 2; }
  push_flag=""
  while IFS=$'\t' read -r line_flag line_refspec _; do
    if [ "$line_refspec" = "$refspec" ]; then
      push_flag=$line_flag
      break
    fi
  done <<<"$push_out"
  case "$push_flag" in
    ' ') push_moved=1 ;;   # fast-forward update
    '+') push_moved=1 ;;   # forced update
    '*') push_moved=1 ;;   # new branch
    '=') push_moved=0 ;;   # up to date: this push changed nothing
    '')
      echo "ci-build: git push printed no status line for $refspec; cannot tell whether it moved the branch" >&2
      exit 2
      ;;
    *)
      echo "ci-build: git push reported '$push_flag' for $refspec; nothing can be concluded" >&2
      exit 2
      ;;
  esac
  echo "pushed $branch ${sha:0:7}; waiting for its build run…"
else
  # --wait-only: wait for the run of the commit the branch already has.
  snapshot=$(matching_ids) || { echo "ci-build: gh run list failed" >&2; exit 2; }
  echo "wait-only: not pushing; waiting for the run of $branch ${sha:0:7}…"
fi
if [ "$trigger" = push ]; then
  # Only a run caused by this push's move; an up-to-date push created none.
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
    if ! printf '%s\n' "$failed_log" | tail -n 40 >&2; then
      echo "ci-build: cannot print the failed-step log tail" >&2
      exit 2
    fi
  else
    echo "ci-build: no failed-step log for run $run_id" >&2
  fi
  exit 1
fi

if [ "$download" = 0 ]; then
  echo "ci-build: $branch ${sha:0:7} green (artifact not downloaded)"
  exit 0
fi

artifacts=ci-artifacts
dest="$artifacts/$sha"
# Beside the destination and on its filesystem, so the final mv is a rename.
staging="$artifacts/.$sha.staging.$$"
if ! tmpdir=$(mktemp -d); then
  echo "ci-build: mktemp -d failed" >&2
  exit 2
fi
# A failure must leave neither a half-written ci-artifacts/<sha> (only the rename at
# the end writes it) nor the scratch directories behind.
trap 'rm -rf -- "$tmpdir" "$staging" || echo "ci-build: could not remove the temporary directories" >&2' EXIT
if ! gh run download "$run_id" --name "$artifact" --dir "$tmpdir"; then
  echo "ci-build: could not download the $artifact artifact of run $run_id" >&2
  exit 2
fi

# Validate the downloaded tree before anything is written to ci-artifacts/<sha>: the
# artifact carries module packages, so its relative layout is kept (two files with one
# basename in different directories must both survive) and only regular files and
# directories are accepted — a symlink, FIFO, socket or device is refused.
if ! special=$(find "$tmpdir" -mindepth 1 ! -type f ! -type d); then
  echo "ci-build: cannot inspect the downloaded $artifact tree" >&2
  exit 2
fi
if [ -n "$special" ]; then
  echo "ci-build: the downloaded $artifact holds an entry that is neither a regular file nor a directory:" >&2
  printf '%s\n' "$special" >&2
  exit 2
fi
if ! top=$(find "$tmpdir" -mindepth 1 -maxdepth 1); then
  echo "ci-build: cannot list the downloaded $artifact tree" >&2
  exit 2
fi

# Locate the artifact root deterministically: `gh run download --name X --dir D`
# extracts the artifact's contents directly into D, so D is the root; a D that holds
# nothing but a single directory named after the artifact is accepted as well, and
# anything else (that directory beside other entries) cannot be located.
count=0
named=0
while IFS= read -r entry; do
  [ -n "$entry" ] || continue
  count=$((count + 1))
  if [ "$entry" = "$tmpdir/$artifact" ] && [ -d "$entry" ]; then
    named=1
  fi
done <<<"$top"
if [ "$named" = 1 ] && [ "$count" -ne 1 ]; then
  echo "ci-build: the downloaded $artifact artifact is a $artifact directory beside other entries in $tmpdir" >&2
  exit 2
fi
if [ "$named" = 1 ]; then
  root=$tmpdir/$artifact
else
  root=$tmpdir
fi
for file in p1 p1.sha256 gate.log; do
  if [ ! -f "$root/$file" ]; then
    echo "ci-build: the $artifact artifact has no $file" >&2
    exit 2
  fi
done

if ! mkdir -p "$artifacts"; then
  echo "ci-build: cannot create $artifacts" >&2
  exit 2
fi
if ! rm -rf -- "$staging"; then
  echo "ci-build: cannot clear $staging" >&2
  exit 2
fi
if ! mkdir -p -- "$staging"; then
  echo "ci-build: cannot create $staging" >&2
  exit 2
fi
if ! cp -a "$root/." "$staging/"; then
  echo "ci-build: cannot stage the $artifact artifact into $staging" >&2
  exit 2
fi
# The rename is the only write to ci-artifacts/<sha>: an earlier download of the same
# commit is replaced cleanly, or nothing is written at all. -T renames the staging
# directory onto $dest itself, so a concurrent invocation for the same commit
# (ADR-0066) that recreated $dest between the rm and this mv fails loudly (exit 2)
# instead of silently nesting the staging directory inside a fresh destination.
if ! rm -rf -- "$dest"; then
  echo "ci-build: cannot replace $dest" >&2
  exit 2
fi
if ! mv -T -- "$staging" "$dest"; then
  echo "ci-build: cannot move the staged artifact to $dest" >&2
  exit 2
fi
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
