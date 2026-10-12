#!/usr/bin/env bash
# Runs p1's author conformance checks (ADR-0210) on every package ./release.sh staged in dist/:
# the suite of the phaseone checkout behind p1-module-sdk installs each archive into a scratch
# configuration with p1's own install path and loads it as a session would (the loader's checks:
# kind, world, protocol, grants, token origins, the class allocation, the digest). The first run
# builds p1's test harness into <target>/p1-conformance; later runs reuse it.
set -euo pipefail
cd "$(dirname "$0")"
compgen -G 'dist/*.p1pkg' > /dev/null || { echo "conformance: no dist/*.p1pkg; run ./release.sh first" >&2; exit 1; }

read -r sdk target < <(cargo metadata --format-version 1 --locked | python3 -c '
import json, os, sys
metadata = json.load(sys.stdin)
sdk = next(p for p in metadata["packages"] if p["name"] == "p1-module-sdk")
print(os.path.dirname(os.path.dirname(os.path.dirname(sdk["manifest_path"]))), metadata["target_directory"])')

for archive in dist/*.p1pkg; do
  echo "== conformance: $archive"
  P1_CONFORMANCE_PACKAGE="$PWD/$archive" CARGO_TARGET_DIR="$target/p1-conformance" \
    cargo test --manifest-path "$sdk/Cargo.toml" --locked -p p1-module-tests --test author_conformance -- --nocapture
done
