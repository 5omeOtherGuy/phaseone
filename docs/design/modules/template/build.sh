#!/usr/bin/env bash
# Builds this module package and packs it into target/p1-module/<crate>.p1pkg, the archive
# `p1 modules install` reads (ADR-0209). The steps are phaseone's own: the scripts of the
# phaseone checkout cargo fetched for the p1-module-sdk dependency build the package exactly as
# p1's packages are built (scripts/build-modules.sh --external) and pack it
# (scripts/pack-module.py). Needs the Rust of rust-toolchain.toml, python3, and wasm-tools at
# the version the SDK pins (modules/toolchain.pins, WASM_TOOLS).
set -euo pipefail
cd "$(dirname "$0")"

# This crate's name and version, and the phaseone checkout behind p1-module-sdk.
cargo fetch --quiet
read -r name version sdk < <(cargo metadata --format-version 1 | python3 -c '
import json, os, sys
metadata = json.load(sys.stdin)
root = next(p for p in metadata["packages"] if p["id"] == metadata["resolve"]["root"])
sdk = next(p for p in metadata["packages"] if p["name"] == "p1-module-sdk")
print(root["name"], root["version"], os.path.dirname(os.path.dirname(os.path.dirname(sdk["manifest_path"]))))')

pinned="$(sed -n 's/^WASM_TOOLS=//p' "$sdk/modules/toolchain.pins")"
case "$(wasm-tools --version 2>/dev/null)" in
  "wasm-tools $pinned"*) ;;
  *) echo "build: wasm-tools $pinned is required (modules/toolchain.pins of the SDK)" >&2; exit 1 ;;
esac

out="$PWD/target/p1-module"
bash "$sdk/scripts/build-modules.sh" --external "$PWD" --output "$out"
python3 "$sdk/scripts/pack-module.py" "$out/$name" --version "$version" --output "$out/$name.p1pkg"
