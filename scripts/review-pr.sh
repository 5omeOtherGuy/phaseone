#!/usr/bin/env bash
# review-pr.sh <pr> <focus-file> — prepare a read-only native review brief.
# Model-cards selects the reviewer/effort; the caller dispatches Agent by default.
# --external-codex --route-reason <limitation/evidence> --model <id> --effort <level>
# explicitly launches the Codex fallback. Sol reviews are medium, never high.
set -euo pipefail
usage() { echo "usage: scripts/review-pr.sh <pr> <focus-file> [--external-codex --route-reason TEXT --model ID --effort LEVEL]" >&2; exit 2; }
[ $# -ge 2 ] || usage
n="$1"; focus="$2"; shift 2
external=false; reason=; model=; effort=
while [ $# -gt 0 ]; do
  case "$1" in
    --external-codex) external=true; shift ;;
    --route-reason|--model|--effort)
      [ $# -ge 2 ] || usage
      case "$1" in --route-reason) reason="$2" ;; --model) model="$2" ;; --effort) effort="$2" ;; esac
      shift 2 ;;
    *) usage ;;
  esac
done
case "$n" in *[!0-9]*|'') usage ;; esac
[ -s "$focus" ] || { echo "review-pr: focus file missing or empty" >&2; exit 2; }
if "$external"; then
  [ -n "${reason//[[:space:]]/}" ] && [ -n "$model" ] && [ -n "$effort" ] || usage
  case "$effort" in low|medium|high|xhigh) ;; *) echo "review-pr: unsupported or unapproved effort" >&2; exit 2 ;; esac
  if [[ "$model" == gpt-6.1-sol* ]] && [ "$effort" != medium ]; then
    echo "review-pr: Sol reviews require medium" >&2; exit 2
  fi
elif [ -n "$reason$model$effort" ]; then
  usage
fi
focus="$(realpath -- "$focus")"
repo="$(cd "$(dirname "$0")/.." && pwd)"
# Never overwrite an earlier review's evidence or move its existing worktree.
mkdir -p "${P1_REVIEW_DIR:-$HOME/.local/state/p1-review}"
dir="$(mktemp -d "${P1_REVIEW_DIR:-$HOME/.local/state/p1-review}/pr-$n-XXXXXXXX")"
dir="$(realpath -- "$dir")"
git -C "$repo" fetch -q origin "pull/$n/head"
head="$(git -C "$repo" rev-parse FETCH_HEAD)"
git -C "$repo" worktree add -q --detach "$dir/tree" "$head"
mkdir -p "$dir/tree/.review"
gh pr view "$n" --json number,title,body,headRefOid,baseRefName,files \
  --jq '"# PR #\(.number): \(.title)\nhead \(.headRefOid) base \(.baseRefName)\nfiles: \([.files[].path] | join(", "))\n\n\(.body)"' \
  > "$dir/tree/.review/pr.md"
# Review exactly the fetched revision, even if the remote PR changes afterwards.
base="$(gh pr view "$n" --json baseRefName --jq .baseRefName)"
git -C "$repo" fetch -q origin "$base"
git -C "$repo" diff "$(git -C "$repo" merge-base FETCH_HEAD "$head")" "$head" > "$dir/tree/.review/pr.diff"
cp -- "$focus" "$dir/tree/.review/focus.md"
cat > "$dir/BRIEF.md" <<BRIEF
Category: review
Workspace: $dir/tree
Owned paths: none; read-only source and .review inputs. Excluded: all writes, builds and tests.
Read first: .review/focus.md, .review/pr.diff, .review/pr.md, then only source needed by focus.
Outcome: source-backed review of PR #$n at $head; required CI gate runs separately.
Permissions: no sudo, edits, builds, tests, commits, pushes or delegation; Bash read-only.
Stay inside focus; outside it report only an observed P0. Do not report compiler/test failures.
Result: final response inline (caller persists if permitted), no report file required.
## P0/P1 — path:line, violated requirement, concrete failing input/scenario; none found is valid.
## P2 — at most five, one line each.
## Verdict — LAND or FIX FIRST and the P0/P1 items that must be fixed.
Then STOP.
BRIEF
printf 'review prepared: PR #%s head %s workspace %s brief %s\n' "$n" "$head" "$dir/tree" "$dir/BRIEF.md"
if ! "$external"; then
  echo "Select via model-cards, then dispatch native read-only Agent with this brief; no reviewer launched."
  exit 0
fi
printf 'route=codex-exec\nmodel=%s\neffort=%s\nreason=%s\nhead=%s\n' "$model" "$effort" "$reason" "$head" > "$dir/route.txt"
setsid nohup codex exec -m "$model" -c "model_reasoning_effort=\"$effort\"" -s read-only \
  -C "$dir/tree" --skip-git-repo-check -o "$dir/review.md" - < "$dir/BRIEF.md" \
  > "$dir/run.log" 2>&1 &
echo "external review started, pid $!, report $dir/review.md; reason recorded in $dir/route.txt"
