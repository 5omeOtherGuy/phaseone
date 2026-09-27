#!/usr/bin/env bash
# Stage p1's four release assets into one directory: the native binary, its sha256, the
# share archive and its sha256 (ADR-0065). This is the one staging path; the release
# workflow and the installed-release test both call it.
#
# Nothing but the component is shipped (docs/design/modules/package.md), so the share
# archive carries environments/, routes/, profiles/ and modules/ with manifest.json and
# packages/<package>/<package>.wasm only: the repository's own modules/ sources are never
# packed, and no build output beside the component (the .wit, .imports, .sha256 and
# .manifest.json the build reads) reaches the archive. modules/manifest.json is written by
# scripts/release-manifest.py, with the components entries filled from the build outputs.
#
# Usage:
#   scripts/stage-release.sh --native <p1 binary> --out <dir> --commit <40 hex>
#                            [--tag <tag>] [--modules <built packages dir>]
#
#   --native   the built p1 binary to ship as p1-linux-x86_64
#   --out      the directory the four assets are written into
#   --commit   the 40-character lowercase hex source commit the release names
#   --tag      the release tag, e.g. main-<12 hex>; omit it for a candidate build
#   --modules  the build outputs, one directory per package as scripts/build-modules.sh
#              publishes them (default modules/target/p1-modules)
#
# Everything is staged in a temporary directory beside --out and moved into place only
# when the whole archive is complete, so a failure never leaves a partial --out. A
# compiled-cache blob, a symlink, an unexpected build output and a package whose .sha256
# does not match its .wasm are refused: no precompiled component is ever shipped.
#
# Exit codes: 0 when the four assets are written, 1 on a rejected input, 2 on a usage error.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

usage() {
  cat <<'EOF'
usage: scripts/stage-release.sh --native <p1 binary> --out <dir> --commit <40 hex>
                                [--tag <tag>] [--modules <built packages dir>]

--native   the built p1 binary to ship as p1-linux-x86_64
--out      the directory the four assets are written into
--commit   the 40-character lowercase hex source commit the release names
--tag      the release tag, e.g. main-<12 hex>; omit it for a candidate build
--modules  the build outputs (default modules/target/p1-modules)

The four assets are p1-linux-x86_64, p1-linux-x86_64.sha256, p1-share.tar.gz and
p1-share.tar.gz.sha256.
Exit 0 when the four assets are written, 1 on a rejected input, 2 on a usage error.
EOF
}

usage_error() {
  usage >&2
  exit 2
}

fail() {
  echo "stage-release: $*" >&2
  exit 1
}

native=""
out=""
commit=""
tag=""
modules="modules/target/p1-modules"

while [ $# -gt 0 ]; do
  case "$1" in
    --native)
      [ $# -ge 2 ] || usage_error
      native="$2"
      shift 2
      ;;
    --out)
      [ $# -ge 2 ] || usage_error
      out="$2"
      shift 2
      ;;
    --commit)
      [ $# -ge 2 ] || usage_error
      commit="$2"
      shift 2
      ;;
    --tag)
      [ $# -ge 2 ] || usage_error
      tag="$2"
      shift 2
      ;;
    --modules)
      [ $# -ge 2 ] || usage_error
      modules="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "stage-release: unknown argument: $1 (try --help)" >&2
      usage_error
      ;;
  esac
done

[ -n "$native" ] || usage_error
[ -n "$out" ] || usage_error
[ -n "$commit" ] || usage_error

[ -f "$native" ] || fail "--native $native: missing"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] ||
  fail "--commit $commit: expected 40 lowercase hex characters"
[ -d "$modules" ] || fail "--modules $modules: not a directory"

out="$(realpath -m -- "$out")" || fail "--out $out: cannot be resolved"
[ "$out" != "/" ] || fail "--out must name a directory, not /"
out_parent="$(dirname -- "$out")"
if [ -e "$out" ] && [ ! -d "$out" ]; then
  fail "--out $out: exists and is not a directory"
fi

# The share tree is assembled in a scratch directory and packed into the staging directory
# that becomes --out; both are below the caller's filesystem so the final move is a rename.
mkdir -p -- "$out_parent"
work="$(mktemp -d "$out_parent/.p1-release.XXXXXX")" ||
  fail "--out $out: cannot create a staging directory beside it"
share="$(mktemp -d "${TMPDIR:-/tmp}/p1-share.XXXXXX")" ||
  fail "cannot create a scratch share directory"
cleanup() {
  rm -rf -- "$work" "$share"
}
trap cleanup EXIT
# An interrupt kills the shell without running the EXIT trap, so clean up explicitly.
trap 'cleanup; exit 130' INT TERM

mkdir -p -- "$share/modules/packages"
for dir in environments routes profiles; do
  [ -d "$dir" ] || fail "$dir/: missing from the checkout"
  cp -a -- "$dir" "$share/"
done

install -m 0755 -- "$native" "$work/p1-linux-x86_64"

# One package directory per build output. Only <package>.wasm is shipped; the build's
# other four files are read here and never packed.
packages=0
shopt -s nullglob
for entry in "$modules"/*; do
  package="$(basename -- "$entry")"
  # scripts/build-modules.sh writes the development manifest (BLOCKERS S3-B6, D080) at the top
  # of the directory it published the packages into, and that is the same directory this script
  # reads: it sits beside the packages and is not one.
  [ "$package" = manifest.json ] && continue
  [ -d "$entry" ] && [ ! -L "$entry" ] ||
    fail "$modules/$package: not a package directory"
  for file in "$entry"/*; do
    base="$(basename -- "$file")"
    case "$base" in
      *.cwasm) fail "$package/$base: a compiled-cache blob is never shipped" ;;
    esac
    [ ! -L "$file" ] || fail "$package/$base: a symlink is never shipped"
    [ -f "$file" ] || fail "$package/$base: not a regular file"
    case "$base" in
      "$package".wasm | "$package".wit | "$package".sha256 | "$package".imports | "$package".manifest.json) ;;
      *) fail "$package/$base: not a file the module package format ships" ;;
    esac
  done

  wasm="$entry/$package.wasm"
  sha="$entry/$package.sha256"
  package_manifest="$entry/$package.manifest.json"
  [ -f "$wasm" ] || fail "$package: no $package.wasm under $modules"
  [ -f "$sha" ] || fail "$package: no $package.sha256 under $modules"
  [ -f "$package_manifest" ] || fail "$package: no $package.manifest.json under $modules"

  want="$(awk 'NF { print $1; exit }' "$sha")"
  got="$(sha256sum "$wasm" | awk '{ print $1 }')"
  [ -n "$want" ] || fail "$package: $package.sha256 names no digest"
  [ "$want" = "$got" ] ||
    fail "$package: $package.sha256 says $want, $package.wasm is $got"

  name="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["name"])' "$package_manifest")" ||
    fail "$package: cannot read the package name from $package_manifest"
  case "$name" in
    */*) ;;
    *) fail "$package: name $name is not <namespace>/<name>" ;;
  esac
  file="${name//\//-}"
  mkdir -p -- "$share/modules/packages/$file"
  install -m 0644 -- "$wasm" "$share/modules/packages/$file/$file.wasm"
  packages=$((packages + 1))
done
shopt -u nullglob

[ "$packages" -gt 0 ] ||
  fail "$modules: no module packages to ship (run scripts/build-modules.sh --all)"

manifest_args=(--root "$root" --commit "$commit" --native "$work/p1-linux-x86_64"
  --modules-dir "$share/modules" --build-modules-dir "$modules")
[ -z "$tag" ] || manifest_args+=(--tag "$tag")
python3 "$root/scripts/release-manifest.py" "${manifest_args[@]}"

tar -czf "$work/p1-share.tar.gz" -C "$share" environments routes profiles modules
(cd "$work" && sha256sum p1-linux-x86_64 >p1-linux-x86_64.sha256)
(cd "$work" && sha256sum p1-share.tar.gz >p1-share.tar.gz.sha256)

# The four assets are complete, so the old --out (if any) is replaced in one step.
rm -rf -- "$out"
mv -- "$work" "$out"
echo "stage-release: $out (p1-linux-x86_64, p1-linux-x86_64.sha256, p1-share.tar.gz, p1-share.tar.gz.sha256)"
