#!/usr/bin/env bash
# CI gate: catch credential-shaped strings before they land. Scans every file tracked
# by git (via `git ls-files`, so it works from any cwd inside the work tree) for the
# `sk-` shapes from fleet PR #51's canonical pattern (rule 2 of the lead's decision:
# the bash scan uses exactly the `sk-` part). A hit prints only `file:line` — never the
# matched text — so the gate output itself is never a leak.
set -euo pipefail

pattern='sk-([A-Za-z0-9]{20,}|(ant|proj|or|svcacct|admin)-[A-Za-z0-9_-]{20,})|\bsk-[A-Za-z0-9_-]{20,}'

found=0
while IFS= read -r -d '' file; do
  [ -f "$file" ] || continue
  lines="$(grep -nIE -e "$pattern" -- "$file" 2>/dev/null | cut -d: -f1 || true)"
  [ -n "$lines" ] || continue
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    printf '%s:%s\n' "$file" "$line"
    found=1
  done <<< "$lines"
done < <(git ls-files -z)

if [ "$found" -eq 1 ]; then
  echo "secret-scan: credential-shaped strings found above (values are never shown)" >&2
  exit 1
fi

echo "secret-scan: clean"
exit 0
