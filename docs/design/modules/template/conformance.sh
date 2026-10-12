#!/usr/bin/env bash
# Runs p1's author conformance checks on the package ./build.sh built (ADR-0210): the suite of
# the phaseone checkout behind p1-module-sdk (crates/p1-module-tests/tests/author_conformance.rs)
# installs the archive into a scratch configuration with p1's own install path, loads it as a
# session would and checks it against its class's contract. A provider names its route, its
# model profile and, unless its route speaks one of p1's dialects, its SSE fixtures:
#
#   P1_CONFORMANCE_ROUTE=route.toml P1_CONFORMANCE_PROFILE=profile.toml \
#   P1_CONFORMANCE_FIXTURES=fixtures/ ./conformance.sh
#
# The first run builds p1's test harness (several minutes); later runs reuse it.
set -euo pipefail
cd "$(dirname "$0")"
read -r name sdk < <(cargo metadata --format-version 1 | python3 -c '
import json, os, sys
metadata = json.load(sys.stdin)
root = next(p for p in metadata["packages"] if p["id"] == metadata["resolve"]["root"])
sdk = next(p for p in metadata["packages"] if p["name"] == "p1-module-sdk")
print(root["name"], os.path.dirname(os.path.dirname(os.path.dirname(sdk["manifest_path"]))))')
archive="$PWD/target/p1-module/$name.p1pkg"
[ -f "$archive" ] || { echo "conformance: $archive is missing; run ./build.sh first" >&2; exit 1; }
for variable in P1_CONFORMANCE_ROUTE P1_CONFORMANCE_PROFILE P1_CONFORMANCE_FIXTURES; do
  value="${!variable:-}"
  case "$value" in "" | /*) ;; *) export "$variable=$PWD/$value" ;; esac
done
P1_CONFORMANCE_PACKAGE="$archive" CARGO_TARGET_DIR="$PWD/target/p1-conformance" \
  cargo test --manifest-path "$sdk/Cargo.toml" --locked -p p1-module-tests --test author_conformance
