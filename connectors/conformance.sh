#!/usr/bin/env bash
# Runs p1's author conformance checks (ADR-0210) on every package ./release.sh staged in dist/:
# the suite of the phaseone checkout behind p1-module-sdk installs each archive into a scratch
# configuration with p1's own install path, loads it as a session would and checks it against
# its class's contract. A package whose route, profile or fixtures the suite needs names them in
# <package dir>/conformance.env (P1_CONFORMANCE_ROUTE=…, paths relative to that directory).
# The first run builds p1's test harness; later runs reuse target/p1-conformance.
set -euo pipefail
cd "$(dirname "$0")"
compgen -G 'dist/*.p1pkg' > /dev/null || { echo "conformance: no dist/*.p1pkg; run ./release.sh first" >&2; exit 1; }

mapfile -t packages < <(grep -l '^\[package\.metadata\.p1-module\]' --include=Cargo.toml -r . |
  grep -v '/target/' | xargs -r -n1 dirname | sed 's|^\./||' | sort)
sdk="$(cd "${packages[0]}" && cargo metadata --format-version 1 | python3 -c '
import json, os, sys
metadata = json.load(sys.stdin)
sdk = next(p for p in metadata["packages"] if p["name"] == "p1-module-sdk")
print(os.path.dirname(os.path.dirname(os.path.dirname(sdk["manifest_path"]))))')"

for package in "${packages[@]}"; do
  name="$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["package"]["name"])' "$package/Cargo.toml")"
  (
    unset P1_CONFORMANCE_ROUTE P1_CONFORMANCE_PROFILE P1_CONFORMANCE_FIXTURES
    if [ -f "$package/conformance.env" ]; then
      while IFS='=' read -r key value; do
        case "$key" in
          P1_CONFORMANCE_ROUTE | P1_CONFORMANCE_PROFILE | P1_CONFORMANCE_FIXTURES)
            export "$key=$PWD/$package/$value" ;;
          '' | '#'*) ;;
          *) echo "conformance: $package/conformance.env: unknown key $key" >&2; exit 1 ;;
        esac
      done < "$package/conformance.env"
    fi
    echo "== conformance: $name"
    P1_CONFORMANCE_PACKAGE="$PWD/dist/$name.p1pkg" CARGO_TARGET_DIR="$PWD/target/p1-conformance" \
      cargo test --manifest-path "$sdk/Cargo.toml" --locked -p p1-module-tests --test author_conformance -- --nocapture
  )
done
