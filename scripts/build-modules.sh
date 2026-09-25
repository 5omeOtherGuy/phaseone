#!/usr/bin/env bash
# Builds the module packages under modules/. Discovery is here now so the gate hook is a
# no-op until the first package lands; building is slice S0.4's.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
usage: scripts/build-modules.sh [--all | --package <name>]
       scripts/build-modules.sh --help

--all             build every module package under modules/ (the default)
--package <name>  build only modules/<name>/
A module package is a directory modules/<name>/ containing Cargo.toml.
EOF
}

# Prints the name of every module package, sorted. modules/wit/ holds the WIT worlds and
# modules/Cargo.toml is the workspace root, so neither is ever a package.
module_packages() {
  local dir name
  [ -d modules ] || return 0
  for dir in modules/*/; do
    [ -d "$dir" ] || continue
    name="$(basename "$dir")"
    [ "$name" = wit ] && continue
    [ -f "$dir/Cargo.toml" ] && echo "$name"
  done | LC_ALL=C sort
}

package=""
case "$#" in
  0) ;;
  1)
    case "$1" in
      --all) ;;
      --help | -h)
        usage
        exit 0
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    ;;
  2)
    if [ "$1" = --package ] && [ -n "$2" ]; then
      package="$2"
    else
      usage >&2
      exit 2
    fi
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

mapfile -t packages < <(module_packages)

if [ -n "$package" ]; then
  found=""
  for p in "${packages[@]}"; do
    [ "$p" = "$package" ] && found=1
  done
  if [ -z "$found" ]; then
    echo "build-modules: no module package named $package under modules/"
    exit 1
  fi
  packages=("$package")
elif [ "${#packages[@]}" -eq 0 ]; then
  echo "build-modules: no module packages under modules/; nothing to build"
  exit 0
fi

echo "build-modules: building packages is not implemented yet (slice S0.4)"
exit 1
