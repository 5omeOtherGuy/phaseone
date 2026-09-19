#!/usr/bin/env bash
# Acceptance (seams.md §10): the agent core builds and tests without concrete
# provider, tool, storage or UI modules. Enforced on the resolved dependency graph:
# p1-core's normal dependencies may include only p1-contracts among workspace crates,
# and none of the forbidden third-party crates (HTTP, TLS, terminal, file-format stores).
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -d crates/p1-core ]; then
  echo "core isolation: crates/p1-core not present yet (skipped)"
  exit 0
fi

tree="$(cargo tree --locked -p p1-core -e normal --prefix none | sort -u)"
bad_workspace="$(echo "$tree" | grep -E '^p1-' | grep -vE '^p1-(core|contracts) ' || true)"
bad_external="$(echo "$tree" | grep -E '^(reqwest|hyper|rustls|native-tls|ratatui|crossterm|rusqlite|ignore|grep) ' || true)"

if [ -n "$bad_workspace$bad_external" ]; then
  echo "core isolation VIOLATED — p1-core depends on:"
  echo "$bad_workspace"
  echo "$bad_external"
  exit 1
fi
echo "core isolation: ok"
