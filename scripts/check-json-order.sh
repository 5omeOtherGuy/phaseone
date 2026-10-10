#!/usr/bin/env bash
# The module workspace must not silently switch serde_json maps to insertion order (#689).
set -euo pipefail
cd "$(dirname "$0")/.."

# Include target-specific dependencies too: the shipped guests compile for WASM, not the host.
tree="$(cargo tree --locked --manifest-path modules/Cargo.toml --workspace --target all -e features -i serde_json)"
if [[ "$tree" == *'serde_json feature "preserve_order"'* ]]; then
  echo 'JSON order: a dependency enabled serde_json/preserve_order in modules; see https://github.com/5omeOtherGuy/phaseone/issues/689' >&2
  exit 1
fi
echo 'JSON order: modules use sorted serde_json maps'
