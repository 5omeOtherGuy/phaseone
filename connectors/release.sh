#!/usr/bin/env bash
# Builds every connector module package and stages a release into dist/ (#893): one package
# archive per module (`p1 modules install` reads it, ADR-0209), connectors-catalog.tar.gz (the
# catalog root a p1 distribution anchors: subscriptions/routes and subscriptions/profiles) and
# SHA256SUMS over all of them, the sums phaseone's connectors.lock pins
# (scripts/fetch-connectors.py --pin). The build steps are phaseone's own, from the checkout
# cargo fetched for the p1-module-sdk dependency: scripts/build-modules.sh --external builds a
# package exactly as p1's are built, and scripts/pack-module.py packs it.
#
#   ./release.sh [--version <version>]    default: each package's own Cargo version
#
# A package is a workspace member (subscriptions/components/*) whose Cargo.toml carries
# [package.metadata.p1-module]; its archive is named after its package name with `/` as `-`
# (connectors/glm -> connectors-glm.p1pkg). Needs Rust with the wasm32-unknown-unknown target,
# python3, and wasm-tools at the SDK checkout's pin (modules/toolchain.pins).
set -euo pipefail
cd "$(dirname "$0")"

version=""
case "$#:${1:-}" in
  0:) ;;
  2:--version) version="${2:?--version needs a value}" ;;
  *) echo "usage: ./release.sh [--version <version>]" >&2; exit 2 ;;
esac

# The phaseone checkout behind p1-module-sdk: <checkout>/modules/sdk/Cargo.toml.
sdk="$(cargo metadata --format-version 1 --locked | python3 -c '
import json, os, sys
metadata = json.load(sys.stdin)
sdk = next(p for p in metadata["packages"] if p["name"] == "p1-module-sdk")
print(os.path.dirname(os.path.dirname(os.path.dirname(sdk["manifest_path"]))))')"
pinned="$(sed -n 's/^WASM_TOOLS=//p' "$sdk/modules/toolchain.pins")"
case "$(wasm-tools --version 2>/dev/null)" in
  "wasm-tools $pinned"*) ;;
  *) echo "release: wasm-tools $pinned is required (modules/toolchain.pins of the SDK checkout)" >&2; exit 1 ;;
esac

# Every package as `<crate> <version> <module name> <directory>`, from Cargo, which resolves the
# workspace-inherited version.
mapfile -t packages < <(cargo metadata --format-version 1 --no-deps --locked | python3 -c '
import json, os, sys
metadata = json.load(sys.stdin)
print(metadata["target_directory"])
for package in sorted(metadata["packages"], key=lambda package: package["name"]):
    module = (package.get("metadata") or {}).get("p1-module")
    if module:
        print(package["name"], package["version"], module["name"], os.path.dirname(package["manifest_path"]))')
target="${packages[0]}"
packages=("${packages[@]:1}")
[ "${#packages[@]}" -gt 0 ] || { echo "release: no module package in the workspace" >&2; exit 1; }

rm -rf dist "$target/p1-module"
mkdir -p dist
for package in "${packages[@]}"; do
  read -r crate own module dir <<<"$package"
  bash "$sdk/scripts/build-modules.sh" --external "$dir" --output "$target/p1-module"
  python3 "$sdk/scripts/pack-module.py" "$target/p1-module/$crate" --version "${version:-$own}" \
    --output "dist/${module//\//-}.p1pkg"
done

# The catalog archive: data only, reproducible (sorted members, no owner, mtime 0).
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 --mode=a-st,u+w,go-w \
  -C subscriptions -cf - routes profiles | gzip -n > dist/connectors-catalog.tar.gz
(cd dist && sha256sum ./*.p1pkg connectors-catalog.tar.gz | sed 's|  \./|  |' > SHA256SUMS)
echo "release: ${#packages[@]} packages and the catalog in dist/"
cat dist/SHA256SUMS
