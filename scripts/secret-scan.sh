#!/usr/bin/env bash
# CI gate: catch credential-shaped strings before they land. Scans every file tracked
# by git (via `git ls-files`, so it works from any cwd inside the work tree) for the
# `sk-` shapes from fleet PR #51's canonical pattern (rule 2 of the lead's decision:
# the bash scan uses exactly the `sk-` part). A hit prints only `file:line` — never the
# matched text — so the gate output itself is never a leak.
set -euo pipefail

pattern='sk-([A-Za-z0-9]{20,}|(ant|proj|or|svcacct|admin)-[A-Za-z0-9_-]{20,})|\bsk-[A-Za-z0-9_-]{20,}'

tracked="$(mktemp "${TMPDIR:-/tmp}/p1-secret-scan.XXXXXX")"
trap 'rm -f -- "$tracked"' EXIT
if ! git ls-files -z >"$tracked"; then
  echo "secret-scan: cannot enumerate tracked files" >&2
  exit 1
fi

found=0
while IFS= read -r -d '' file; do
  if [ -L "$file" ] || [ ! -f "$file" ] || [ ! -r "$file" ]; then
    printf 'secret-scan: cannot inspect tracked entry %s\n' "$file" >&2
    found=1
    continue
  fi
  # grep -I silently skips binary data; reject it instead of claiming a clean scan.
  if ! python3 - "$file" <<'PY'
import sys
with open(sys.argv[1], 'rb') as source:
    for chunk in iter(lambda: source.read(1024 * 1024), b''):
        if b'\0' in chunk:
            raise SystemExit(1)
PY
  then
    printf 'secret-scan: binary or unreadable tracked entry %s\n' "$file" >&2
    found=1
    continue
  fi
  status=0
  matches="$(grep -nE -e "$pattern" -- "$file")" || status=$?
  if [ "$status" -gt 1 ]; then
    printf 'secret-scan: grep failed on %s\n' "$file" >&2
    found=1
    continue
  fi
  [ -n "$matches" ] || continue
  lines="$(printf '%s\n' "$matches" | cut -d: -f1)"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    printf '%s:%s\n' "$file" "$line"
    found=1
  done <<< "$lines"
done <"$tracked"

if [ "$found" -eq 1 ]; then
  echo "secret-scan: credential-shaped strings found above (values are never shown)" >&2
  exit 1
fi

echo "secret-scan: clean"
exit 0
