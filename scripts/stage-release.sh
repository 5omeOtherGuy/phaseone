#!/usr/bin/env bash
# Stage p1's four release assets into one directory: the native binary, its sha256, the
# share archive and its sha256 (ADR-0065). This is the one staging path; the release
# workflow and the installed-release test both call it.
#
# Nothing but the component and its compiled copy is shipped (docs/design/modules/package.md,
# ADR-0113), so the share archive carries environments/, routes/, profiles/, accounts/ and modules/ with
# manifest.json and packages/<package>/<package>.wasm and .cwasm only: the repository's own
# modules/ sources are never packed, and no build output beside the component (the .wit,
# .imports, .sha256 and .manifest.json the build reads) reaches the archive. The .cwasm is
# compiled here by the binary this release ships (`p1 modules precompile`), so it comes from
# that binary's own wasmtime and engine configuration. modules/manifest.json is written by
# scripts/release-manifest.py, with the components entries filled from the build outputs.
#
# Usage:
#   scripts/stage-release.sh --native <p1 binary> --out <dir> --commit <40 hex>
#                            [--tag <tag>] [--modules <built packages dir>]
#                            [--parts <part,...> | --catalog-root <path> ...]
#                            [--asset-name <name>]
#
#   --native   the built p1 binary to ship as <name>-linux-x86_64
#   --out      the directory the four assets are written into
#   --commit   the 40-character lowercase hex source commit the release names
#   --tag      the release tag, e.g. main-<12 hex>; omit it for a candidate build
#   --modules  the build outputs, one directory per package as scripts/build-modules.sh
#              publishes them (default modules/target/p1-modules)
#   --parts    the optional parts composed over the core catalog (#855, ADR-0207), from
#              `subscriptions` (connectors/subscriptions: its catalog and its module packages,
#              ADR-0214) and `owner` (packs/owner); empty or absent ships the core alone. The
#              binary must anchor the same roots: P1_CATALOG_ROOTS names them, and unset it is
#              the core and connectors/subscriptions
#   --catalog-root  repeatable source catalog roots (default .), instead of --parts; pack
#                   exactly the roots the native binary anchored (ADR-0196)
#   --asset-name    the asset set's name (default p1): <name>-linux-x86_64 and
#                   <name>-share.tar.gz with their .sha256
#
# Everything is staged in a temporary directory beside --out and moved into place only
# when the whole archive is complete, so a failure never leaves a partial --out. A
# compiled file among the build outputs, a symlink, an unexpected build output and a package
# whose .sha256 does not match its .wasm are refused: the only compiled copies shipped are
# the ones --native compiles from the staged components.
#
# Exit codes: 0 when the four assets are written, 1 on a rejected input, 2 on a usage error.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

usage() {
  cat <<'EOF'
usage: scripts/stage-release.sh --native <p1 binary> --out <dir> --commit <40 hex>
                                [--tag <tag>] [--modules <built packages dir>]
                                [--parts <part,...> | --catalog-root <path> ...]
                                [--asset-name <name>]

--native   the built p1 binary to ship as <name>-linux-x86_64
--out      the directory the four assets are written into
--commit   the 40-character lowercase hex source commit the release names
--tag      the release tag, e.g. main-<12 hex>; omit it for a candidate build
--modules  the build outputs (default modules/target/p1-modules)
--parts    parts composed over the core catalog: subscriptions, owner (comma-separated;
           empty or absent: the core alone); the binary must anchor the same roots
--catalog-root  catalog root, relative to the repository or absolute (repeatable;
                default .), instead of --parts; must match the binary's anchor (ADR-0196)
--asset-name    the asset set's name (default p1)

The four assets are <name>-linux-x86_64, <name>-linux-x86_64.sha256, <name>-share.tar.gz
and <name>-share.tar.gz.sha256.
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
catalog_args=()
parts=""
parts_given=0
name="p1"

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
    --catalog-root)
      [ $# -ge 2 ] || usage_error
      catalog_args+=(--catalog-root "$2")
      shift 2
      ;;
    --parts)
      [ $# -ge 2 ] || usage_error
      parts="$2"
      parts_given=1
      shift 2
      ;;
    --asset-name)
      [ $# -ge 2 ] || usage_error
      name="$2"
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
[[ "$name" =~ ^p1(-[a-z0-9]+)*$ ]] || {
  echo "stage-release: --asset-name $name: expected p1 or p1-<word>" >&2
  usage_error
}

# The parts a release composes over the core catalog (ADR-0207), in this fixed order: the
# core first, then the connectors, then the owner pack, as the binary's anchor lists them.
if [ "$parts_given" -eq 1 ]; then
  [ "${#catalog_args[@]}" -eq 0 ] || {
    echo "stage-release: --parts and --catalog-root name the roots twice; give one" >&2
    usage_error
  }
  catalog_args=(--catalog-root .)
  selected=",$parts,"
  for part in ${parts//,/ }; do
    case "$part" in
      subscriptions | owner) ;;
      *)
        echo "stage-release: --parts $parts: unknown part $part (subscriptions, owner)" >&2
        usage_error
        ;;
    esac
  done
  [[ "$selected" != *,subscriptions,* ]] || catalog_args+=(--catalog-root connectors/subscriptions)
  [[ "$selected" != *,owner,* ]] || catalog_args+=(--catalog-root packs/owner)
fi

# The connectors' module packages (ADR-0214): each connectors part built its packages beside
# p1's own (`scripts/build-modules.sh --all`, one directory per crate). A share ships those of
# the parts it composes, in the release set under their own namespace, where the host finds them
# by name; the packages of a part it leaves out are not shipped.
declare -A core_crate=() connector_crate=() selected_part=()
# Match build-modules.sh's module_packages: bindings and SDK crates are not packages.
for dir in modules/p1-module-*/; do
  [ -f "$dir/Cargo.toml" ] || continue
  grep -q '^\[package\.metadata\.p1-module\]' "$dir/Cargo.toml" || continue
  core_crate[$(basename -- "$dir")]=1
done
for catalog in "${catalog_args[@]}"; do
  [ "$catalog" != --catalog-root ] || continue
  catalog="${catalog#"$root"/}"
  catalog="${catalog#./}"
  catalog="${catalog%/}"
  case "$catalog" in connectors/*) ;; *) continue ;; esac
  [ -d "$catalog/components" ] || continue
  selected_part[$catalog]=1
done
for manifest in connectors/*/components/*/Cargo.toml; do
  [ -f "$manifest" ] || continue
  grep -q '^\[package\.metadata\.p1-module\]' "$manifest" || continue
  part="${manifest%%/components/*}"
  crate="$(awk '
    /^[[:space:]]*\[/ { inside = ($0 ~ /^\[package\][[:space:]]*$/); next }
    inside && /^[[:space:]]*name[[:space:]]*=/ {
      value = $0; sub(/^[^=]*=[[:space:]]*"/, "", value); sub(/".*$/, "", value); print value; exit
    }' "$manifest")"
  connector_crate[$crate]="$part"
done
binary_asset="$name-linux-x86_64"
share_asset="$name-share.tar.gz"
assets=("$binary_asset" "$binary_asset.sha256" "$share_asset" "$share_asset.sha256")

[ -f "$native" ] || fail "--native $native: missing"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] ||
  fail "--commit $commit: expected 40 lowercase hex characters"
[ -d "$modules" ] || fail "--modules $modules: not a directory"

[ ! -L "$out" ] || fail "--out is a symlink"
out="$(realpath -m -- "$out")" || fail "--out $out: cannot be resolved"
[ "$out" != "/" ] || fail "--out must name a directory, not /"
out_parent="$(dirname -- "$out")"
# One --out at a time: the ownership record and the directory swap below form one decision,
# so two invocations replacing the same --out must not interleave (Codex finding
# stage-release.sh:286). Hold the lock for the whole run; the EXIT path releases it.
mkdir -p -- "$out_parent"
lock_file="$out_parent/.$(basename -- "$out").p1-stage-lock"
exec 9>"$lock_file"
flock -x 9
# Ownership is recorded outside the public four-asset directory so its layout stays fixed.
owner_record="$out_parent/.$(basename -- "$out").p1-stage-owner"
# A directory at the record path would silently swallow the record file and leave the
# published stage unowned, so refuse it before anything is staged.
[ ! -d "$owner_record" ] || fail "$owner_record: an ownership record cannot be a directory"
if [ -e "$out" ]; then
  [ -d "$out" ] && [ ! -L "$out" ] || fail "--out $out: not a regular directory"
  # Never remove a directory unless its complete asset set identifies an earlier stage.
  for asset in "${assets[@]}"; do
    [ -f "$out/$asset" ] && [ ! -L "$out/$asset" ] || fail "--out $out: not a staged release"
  done
  [ "$(find "$out" -mindepth 1 -maxdepth 1 | wc -l)" -eq 4 ] ||
    fail "--out $out: contains unrelated entries"
  [ -f "$owner_record" ] && [ ! -L "$owner_record" ] &&
    (cd "$out" && sha256sum -c "$owner_record" >/dev/null 2>&1) ||
    fail "--out $out: no matching prior stage ownership record"
fi

# The share tree is assembled in a scratch directory and packed into the staging directory
# that becomes --out; both are below the caller's filesystem so the final move is a rename.
work="$(mktemp -d "$out_parent/.p1-release.XXXXXX")" ||
  fail "--out $out: cannot create a staging directory beside it"
share=""
backup=""
published=0
owner_temp=""
built=""
cleanup() {
  # Disable errexit: this runs from the EXIT trap, and one failing best-effort step
  # must not skip the restore below.
  set +e
  # Only a directory this process renamed into --out may be removed. A foreign or
  # concurrent process can create --out at any time (the initial check cannot see it),
  # and its directory must never be deleted merely because it lacks this run's record
  # (Codex finding stage-release.sh:152).
  if [ "$published" -eq 1 ]; then
    # Until the new ownership record is in place, a stage in --out is not an owned
    # release: remove our incomplete stage. A stage whose record still verifies is
    # left exactly as it is.
    if [ ! -d "$out" ] || [ -L "$out" ] ||
      ! (cd "$out" 2>/dev/null && sha256sum -c "$owner_record" >/dev/null 2>&1); then
      rm -rf -- "$out"
    fi
  fi
  # Put a prior complete stage moved aside back once --out is free, so a failure or an
  # interrupt never loses a valid release to an unfinished replacement.
  if [ -n "$backup" ] && [ -d "$backup" ] && [ ! -e "$out" ] && [ ! -L "$out" ]; then
    mv -T -- "$backup" "$out" || echo "stage-release: restore failed: $backup" >&2
    backup=""
  fi
  rm -rf -- "$work"
  [ -z "$share" ] || rm -rf -- "$share"
  [ -z "$owner_temp" ] || rm -f -- "$owner_temp"
  [ -z "$built" ] || rm -rf -- "$built"
}
trap cleanup EXIT
share="$(mktemp -d "${TMPDIR:-/tmp}/p1-share.XXXXXX")" ||
  fail "cannot create a scratch share directory"
# An interrupt kills the shell without running the EXIT trap, so clean up explicitly.
trap 'cleanup; exit 130' INT TERM

mkdir -p -- "$share/modules/packages"
python3 "$root/scripts/catalog-roots.py" --repo-root "$root" --out "$share" "${catalog_args[@]}" ||
  fail "cannot merge catalog roots"

install -m 0755 -- "$native" "$work/$binary_asset"

# The build outputs this share ships: p1's own and the selected parts' connector packages,
# copied aside so the manifest generator below reads exactly them.
built="$(mktemp -d "${TMPDIR:-/tmp}/p1-built.XXXXXX")" ||
  fail "cannot create a scratch build outputs directory"
shopt -s nullglob
for entry in "$modules"/*; do
  package="$(basename -- "$entry")"
  [ "$package" != manifest.json ] || continue
  [ -d "$entry" ] && [ ! -L "$entry" ] ||
    fail "$modules/$package: not a package directory"
  part="${connector_crate[$package]:-}"
  if [ -n "$part" ]; then
    [ -n "${selected_part[$part]:-}" ] || continue
  else
    [ -n "${core_crate[$package]:-}" ] || fail "unknown module package $package under $modules"
  fi
  cp -R -- "$entry" "$built/$package"
done
shopt -u nullglob
missing=()
for package in "${!connector_crate[@]}"; do
  part="${connector_crate[$package]}"
  [ -n "${selected_part[$part]:-}" ] || continue
  [ -d "$built/$package" ] || missing+=("$package")
done
[ "${#missing[@]}" -eq 0 ] ||
  fail "missing module packages for selected parts: $(printf '%s\n' "${missing[@]}" | LC_ALL=C sort | paste -sd ',' -) (run scripts/build-modules.sh --all)"
modules="$built"

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

# ADR-0113: the shipped binary writes each staged component's compiled copy beside it;
# scripts/release-manifest.py pins both digests and refuses a package without its copy.
"$(realpath -- "$native")" modules precompile --root "$share/modules" >/dev/null ||
  fail "--native $native: cannot compile the staged packages ahead of time"

manifest_args=(--root "$root" --commit "$commit" --native "$work/$binary_asset"
  --modules-dir "$share/modules" --build-modules-dir "$modules")
[ -z "$tag" ] || manifest_args+=(--tag "$tag")
python3 "$root/scripts/release-manifest.py" "${manifest_args[@]}"

# Normalize tar headers and gzip metadata: a rerun of one source commit must
# produce identical bytes before the release workflow compares remote assets.
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  -cf - -C "$share" environments routes profiles accounts modules | gzip -n >"$work/$share_asset"
# Reconcile the packed bytes, not just the source tree read by the manifest generator.
python3 - "$work/$share_asset" <<'PY' || fail "packed package bytes do not match release manifest"
import hashlib
import json
import sys
import tarfile

with tarfile.open(sys.argv[1], 'r:gz') as archive:
    manifest = json.load(archive.extractfile('modules/manifest.json'))
    for package in manifest['packages']:
        member = archive.getmember('modules/' + package['path'])
        digest = hashlib.sha256()
        size = 0
        with archive.extractfile(member) as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b''):
                size += len(chunk)
                digest.update(chunk)
        if size != package['size'] or digest.hexdigest() != package['sha256']:
            raise SystemExit('package changed between manifest and archive')
PY
(cd "$work" && sha256sum "$binary_asset" >"$binary_asset.sha256")
(cd "$work" && sha256sum "$share_asset" >"$share_asset.sha256")

# The ownership record names the four assets; compute it from the staging directory
# before anything is swapped, so no failure during publication can leave --out without a
# record that matches it (or clobber the record of a stage that is still in place).
owner_temp="$(mktemp "$out_parent/.p1-owner.XXXXXX")"
(cd "$work" && sha256sum "${assets[@]}") >"$owner_temp"

# Preserve the prior complete stage until the replacement has succeeded.
if [ -d "$out" ]; then
  backup="$(mktemp -d "$out_parent/.p1-release-backup.XXXXXX")"
  rmdir -- "$backup"
  mv -T -- "$out" "$backup"
fi
mv -T -- "$work" "$out"
published=1
mv -fT -- "$owner_temp" "$owner_record"
owner_temp=""
if [ -n "$backup" ]; then
  rm -rf -- "$backup" || echo "stage-release: old stage retained at $backup" >&2
  backup=""
fi
echo "stage-release: $out (${assets[0]}, ${assets[1]}, ${assets[2]}, ${assets[3]})"
