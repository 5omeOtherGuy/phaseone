#!/usr/bin/env bash
# Builds every connector module package and stages a release into dist/ (#893): one package
# archive per module (`p1 modules install` reads it, ADR-0209), connectors-catalog.tar.gz (the
# catalog root the distribution binary anchors: subscriptions/routes and subscriptions/profiles)
# and SHA256SUMS over all of them, the sums phaseone's connectors.lock pins
# (scripts/fetch-connectors.py --pin). The build steps are phaseone's own, from the checkout
# cargo fetched for the p1-module-sdk dependency: scripts/build-modules.sh --external builds a
# package exactly as p1's are built, and scripts/pack-module.py packs it.
#
#   ./release.sh [--version <version>]    default: the package's own Cargo version
#
# A module package is a directory whose Cargo.toml carries [package.metadata.p1-module].
# Needs the Rust of rust-toolchain.toml, python3, and wasm-tools at the SDK's pin.
set -euo pipefail
cd "$(dirname "$0")"

version=""
case "${1:-}" in
  --version) version="${2:?--version needs a value}" ;;
  "") ;;
  *) echo "usage: ./release.sh [--version <version>]" >&2; exit 2 ;;
esac

mapfile -t packages < <(grep -l '^\[package\.metadata\.p1-module\]' --include=Cargo.toml -r . |
  grep -v '/target/' | xargs -r -n1 dirname | sed 's|^\./||' | sort)
[ "${#packages[@]}" -gt 0 ] || { echo "release: no module package under $PWD" >&2; exit 1; }

# The phaseone checkout behind p1-module-sdk, from the first package's dependency graph.
sdk_of() {
  (cd "$1" && cargo metadata --format-version 1 | python3 -c '
import json, os, sys
metadata = json.load(sys.stdin)
sdk = next(p for p in metadata["packages"] if p["name"] == "p1-module-sdk")
print(os.path.dirname(os.path.dirname(os.path.dirname(sdk["manifest_path"]))))')
}
sdk="$(sdk_of "${packages[0]}")"
pinned="$(sed -n 's/^WASM_TOOLS=//p' "$sdk/modules/toolchain.pins")"
case "$(wasm-tools --version 2>/dev/null)" in
  "wasm-tools $pinned"*) ;;
  *) echo "release: wasm-tools $pinned is required (modules/toolchain.pins of the SDK)" >&2; exit 1 ;;
esac

rm -rf dist
mkdir -p dist target/p1-module
for package in "${packages[@]}"; do
  read -r name own < <(python3 -c '
import sys, tomllib
package = tomllib.load(open(sys.argv[1], "rb"))["package"]
print(package["name"], package["version"])' "$package/Cargo.toml")
  bash "$sdk/scripts/build-modules.sh" --external "$PWD/$package" --output "$PWD/target/p1-module"
  python3 "$sdk/scripts/pack-module.py" "target/p1-module/$name" --version "${version:-$own}" \
    --output "dist/$name.p1pkg"
done

# The catalog archive: data only, reproducible (sorted members, no owner, mtime 0).
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 --mode=a-st,u+w,go-w \
  -C subscriptions -cf - routes profiles | gzip -n > dist/connectors-catalog.tar.gz
(cd dist && sha256sum ./*.p1pkg connectors-catalog.tar.gz | sed 's|  \./|  |' > SHA256SUMS)
echo "release: ${#packages[@]} packages and the catalog in dist/"
cat dist/SHA256SUMS
