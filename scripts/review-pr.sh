#!/usr/bin/env bash
# review-pr.sh <pr> <focus-file> — one local Codex review of a p1 pull
# request, steered by the lead (ADR-0107). The lead decides whether a PR is reviewed at all; when
# it is, the focus file says what to check (the risky files, the invariants, the failure modes the
# lead worries about) and what to leave (what the gate or an earlier review already covers). The
# reviewer reads the diff and the focus, and stays inside the focus.
#
# Read-only `codex exec` on a detached worktree of the PR head, started detached: the script prints
# the pid and returns. The report lands in <dir>/review.md, where <dir> is
# ${P1_REVIEW_DIR:-$HOME/.local/state/p1-review}/pr-<n>; its last heading is `## Verdict`,
# `LAND` or `FIX FIRST`. GPT-6.1 Sol at effort medium, the effort for reviews
# (OWNER-ORDERS <sol61_worker_20260930>). A review still running after an hour is cut
# (OWNER-ORDERS <landing>).
set -euo pipefail
usage() { echo "usage: scripts/review-pr.sh <pr> <focus-file>" >&2; exit 2; }
[ $# -eq 2 ] || usage
n="$1"; focus="$2"
case "$n" in *[!0-9]*|'') usage ;; esac
[ -s "$focus" ] || { echo "review-pr: focus file $focus is missing or empty" >&2; exit 2; }
focus="$(realpath -- "$focus")"

repo="$(cd "$(dirname "$0")/.." && pwd)"
dir="${P1_REVIEW_DIR:-$HOME/.local/state/p1-review}/pr-$n"
mkdir -p "$dir"
rm -f -- "${dir:?}/review.md"
cd "$repo"
git -C "$repo" fetch -q origin "pull/$n/head:refs/p1-review/pr-$n"
if [ -d "$dir/tree" ]; then
  git -C "$dir/tree" checkout -q --detach "refs/p1-review/pr-$n"
else
  git -C "$repo" worktree add -q --detach "$dir/tree" "refs/p1-review/pr-$n"
fi
mkdir -p "$dir/tree/.review"
gh pr view "$n" --json number,title,body,headRefOid,baseRefName,files \
  --jq '"# PR #\(.number): \(.title)\nhead \(.headRefOid) base \(.baseRefName)\nfiles: \([.files[].path] | join(", "))\n\n\(.body)"' \
  > "$dir/tree/.review/pr.md"
gh pr diff "$n" > "$dir/tree/.review/pr.diff"
cp -- "$focus" "$dir/tree/.review/focus.md"

cat > "$dir/BRIEF.md" <<'BRIEF'
Pre-merge review of one pull request of p1, read-only: you cannot build or run tests, and the
required CI gate (fmt, clippy, every test) runs separately, so do not report what a compiler or a
test run would find.
Material in this directory, the PR head's source tree: `.review/focus.md` is what the lead wants
checked and what to leave; `.review/pr.diff` is the PR's diff; `.review/pr.md` its title, body and
file list. Read focus.md first, then the diff, then only the source the focus needs.
Stay inside the focus. Outside it, report a defect only if it is P0 and you saw it in passing.
Report, in Markdown, as your last message, terse, no preamble, these headings in this order:
## P0/P1 — every defect that breaks correctness, safety, an invariant or the PR's own stated
behaviour: `path:line`, what is wrong, one concrete failing input or scenario. "none found" is valid.
## P2 — at most 5, one line each.
## Verdict — `LAND` or `FIX FIRST`, followed by the P0/P1 items that must be fixed before merge.
BRIEF

cd "$dir"
setsid nohup timeout 3600 codex exec -m gpt-6.1-sol -c 'model_reasoning_effort="medium"' -s read-only \
  -C "$dir/tree" --skip-git-repo-check -o "$dir/review.md" "$(cat "$dir/BRIEF.md")" \
  > "$dir/run.log" 2>&1 < /dev/null &
echo "review of PR #$n started, pid $!, report $dir/review.md"
