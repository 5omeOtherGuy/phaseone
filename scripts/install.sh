#!/usr/bin/env bash
# Install or update p1 from its public release channel (ADR-0062).
#
#   scripts/install.sh [--latest | --from-release TAG | --local] [--prefix DIR]
#
# `--latest` (the default) and `--from-release TAG` download the four published
# release assets — the binary, its sha256, the share tarball, its sha256 — verify both
# checksums, and install:
#
#   <prefix>/bin/p1                                 the binary (0755)
#   <prefix>/share/p1/{environments,routes,profiles}  the shipped data
#   <prefix>/share/p1/install.sh                    this script, so updates need no checkout
#   <prefix>/bin/p1-update                          runs install.sh --latest --prefix <prefix>
#
# Nothing is written before both checksums pass. The binary lands through a temp file
# in <prefix>/bin (a rename within one filesystem), and the share data are unpacked
# beside <prefix>/share/p1 and swapped in, so a failure leaves the previous install
# working.
#
# Nothing under ${XDG_CONFIG_HOME:-$HOME/.config}/p1 — p1's credential store and the
# user's own overrides — is read, written or deleted by this script.
#
# Environment:
#   P1_REPO              the GitHub repository (default 5omeOtherGuy/phaseone)
#   P1_RELEASE_BASE_URL  base for the curl fallback (default the repo's releases page)
#   P1_LOCAL_BUILD_ROOT  the directory --local probes for a build tree (default /mnt/build)
#   CARGO_TARGET_DIR     --local's target directory when set
#   CARGO_BUILD_JOBS     --local's job count (default 2)
set -euo pipefail

BINARY_ASSET="p1-linux-x86_64"
SHARE_ASSET="p1-share.tar.gz"
ASSETS=("$BINARY_ASSET" "$BINARY_ASSET.sha256" "$SHARE_ASSET" "$SHARE_ASSET.sha256")

die() {
  printf 'p1 install: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
usage: install.sh [--latest | --from-release TAG | --local] [--prefix DIR]

  --latest              install the latest published release (default)
  --from-release TAG    install the release tagged TAG
  --local               build this checkout in release profile and install it
  --prefix DIR          where to install (default $HOME/.local)

Environment: P1_REPO, P1_RELEASE_BASE_URL, P1_LOCAL_BUILD_ROOT, CARGO_TARGET_DIR,
CARGO_BUILD_JOBS.
EOF
}

# Where this script came from. `$0` is the interpreter when the script is piped in
# (`curl … | bash`, `bash -s`), so BASH_SOURCE is the honest source: empty means there
# is no file to copy into the share dir, and the install cannot be self-updating.
self_path="${BASH_SOURCE[0]:-}"
if [ -n "$self_path" ]; then
  script_dir="$(cd "$(dirname "$self_path")" && pwd)"
  script="$script_dir/$(basename "$self_path")"
else
  script_dir="$(cd "$(dirname "$0")" && pwd)"
  script=""
fi
repo_root="$(cd "$script_dir/.." && pwd)"

repo="${P1_REPO:-5omeOtherGuy/phaseone}"
base="${P1_RELEASE_BASE_URL:-https://github.com/$repo/releases}"
base="${base%/}"
prefix="${HOME:-/nonexistent}/.local"
mode="latest"
tag=""

while [ $# -gt 0 ]; do
  case "$1" in
    --latest) mode="latest" ;;
    --from-release)
      mode="release"
      [ $# -ge 2 ] || die "--from-release needs a tag"
      tag="$2"
      shift
      ;;
    --local) mode="local" ;;
    --prefix)
      [ $# -ge 2 ] || die "--prefix needs a directory"
      prefix="$2"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
  shift
done

# A literal `~/x` (the shell expands an unquoted `~` only at the start of a word, and
# `--prefix=~/x` expands nothing) is expanded here.
# shellcheck disable=SC2088
case "$prefix" in
  "~" | "~/"*) prefix="${HOME:-/nonexistent}${prefix#"~"}" ;;
esac
case "$prefix" in
  /*) ;;
  *) prefix="$PWD/$prefix" ;;
esac
prefix="${prefix%/}"

stage="$(mktemp -d "${TMPDIR:-/tmp}/p1-install.XXXXXX")"
bin_tmp=""
share_new=""
share_prev="$prefix/share/p1.prev"

cleanup() {
  rm -rf "$stage"
  [ -z "$bin_tmp" ] || rm -f "$bin_tmp"
  [ -z "$share_new" ] || rm -rf "$share_new"
}
trap cleanup EXIT INT TERM

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "no sha256 tool found (need sha256sum or shasum)"
  fi
}

# Refuse anything but an exact match; the caller has not touched the install yet.
verify_asset() {
  local file="$1" sumfile="$2" want got
  [ -f "$file" ] || die "$(basename "$file") was not downloaded"
  [ -f "$sumfile" ] || die "$(basename "$sumfile") was not downloaded"
  want="$(awk 'NF { print $1; exit }' "$sumfile")"
  [ -n "$want" ] || die "$(basename "$sumfile") names no digest"
  got="$(sha256_of "$file")"
  if [ "$got" != "$want" ]; then
    die "$(basename "$file"): sha256 mismatch (expected $want, got $got) — nothing installed"
  fi
  printf 'p1 install: verified %s\n' "$(basename "$file")"
}

download_asset() {
  local asset="$1" url
  if command -v gh >/dev/null 2>&1; then
    printf 'p1 install: gh release download %s\n' "$asset"
    if [ -n "$tag" ]; then
      gh release download "$tag" --repo "$repo" --dir "$stage" --pattern "$asset" --clobber ||
        die "gh could not download $asset from release $tag"
    else
      gh release download --repo "$repo" --dir "$stage" --pattern "$asset" --clobber ||
        die "gh could not download $asset from the latest release"
    fi
  else
    if [ -n "$tag" ]; then
      url="$base/download/$tag/$asset"
    else
      url="$base/latest/download/$asset"
    fi
    printf 'p1 install: curl %s\n' "$url"
    curl -fsSL -o "$stage/$asset" "$url" || die "curl could not download $url"
  fi
}

# The binary through a temp file in <prefix>/bin: a same-filesystem rename, never a
# half-written p1.
install_binary() {
  local src="$1"
  mkdir -p "$prefix/bin"
  bin_tmp="$prefix/bin/.p1.$$"
  cp -f "$src" "$bin_tmp"
  chmod 0755 "$bin_tmp"
  mv -f "$bin_tmp" "$prefix/bin/p1"
  bin_tmp=""
}

# The share data through a temp directory beside <prefix>/share/p1. Unpacking and the
# layout check happen first, so a bad tarball is refused before the binary is replaced;
# the swap itself is a rename, and the previous directory is renamed to p1.prev for it
# and removed once the new one is in place.
stage_share() {
  local tarball="$1" dir
  mkdir -p "$prefix/share"
  share_new="$prefix/share/.p1.new.$$"
  rm -rf "$share_new"
  mkdir -p "$share_new"
  tar -xzf "$tarball" -C "$share_new"
  for dir in environments routes profiles; do
    [ -d "$share_new/$dir" ] || die "$(basename "$tarball") has no $dir/ — nothing installed"
  done
}

swap_share() {
  if [ -e "$prefix/share/p1" ]; then
    rm -rf "$share_prev"
    mv "$prefix/share/p1" "$share_prev"
  fi
  if ! mv "$share_new" "$prefix/share/p1"; then
    [ ! -e "$share_prev" ] || mv "$share_prev" "$prefix/share/p1"
    die "could not swap the share data into $prefix/share/p1"
  fi
  share_new=""
  rm -rf "$share_prev"
}

# Updates need no checkout: the installer lives in the share dir next to the data. The
# staged copy is what is written, because the running script may itself be that file
# and the swap renames and then removes the directory it sat in.
install_self() {
  cp -f "$stage_script" "$prefix/share/p1/install.sh"
  chmod 0755 "$prefix/share/p1/install.sh"
}

install_update_wrapper() {
  local wrapper="$prefix/bin/.p1-update.$$"
  mkdir -p "$prefix/bin"
  cat >"$wrapper" <<EOF
#!/usr/bin/env bash
# Written by p1's install.sh (ADR-0062): update p1 to the latest release.
set -euo pipefail
exec "$prefix/share/p1/install.sh" --latest --prefix "$prefix" "\$@"
EOF
  chmod 0755 "$wrapper"
  mv -f "$wrapper" "$prefix/bin/p1-update"
}

# --local: build this checkout in release profile, never into the repo's target/ —
# a 7 GB-class machine shares one build tree, and the repository's own target dir is
# reserved for the gate.
local_target_dir() {
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    printf '%s\n' "$CARGO_TARGET_DIR"
    return
  fi
  local root="${P1_LOCAL_BUILD_ROOT:-/mnt/build}"
  if [ ! -d "$root" ]; then
    die "--local needs a build directory: set CARGO_TARGET_DIR ($root does not exist here; the repo's target/ is not used)"
  fi
  printf '%s\n' "$root/cargo-target/p1-release"
}

install_local() {
  local target
  [ -f "$repo_root/crates/p1-host/Cargo.toml" ] ||
    die "--local needs a checkout: run scripts/install.sh from the repository"
  target="$(local_target_dir)"
  export CARGO_TARGET_DIR="$target"
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
  printf 'p1 install: cargo build --release --locked -p p1-host (target %s, jobs %s)\n' \
    "$target" "$CARGO_BUILD_JOBS"
  (cd "$repo_root" && cargo build --release --locked -p p1-host)
  local_bin="$target/release/p1"
  [ -x "$local_bin" ] || die "cargo did not produce $local_bin"
  # The share data come from this checkout, so a local build ships this checkout's routes.
  tar -czf "$stage/$SHARE_ASSET" -C "$repo_root" environments routes profiles
  printf 'p1 install: %s -> %s\n' "$local_bin" "$prefix"
}

install_release() {
  local asset
  if [ -n "$tag" ]; then
    printf 'p1 install: release %s -> %s\n' "$tag" "$prefix"
  else
    printf 'p1 install: latest release -> %s\n' "$prefix"
  fi
  for asset in "${ASSETS[@]}"; do
    download_asset "$asset"
  done
  verify_asset "$stage/$BINARY_ASSET" "$stage/$BINARY_ASSET.sha256"
  verify_asset "$stage/$SHARE_ASSET" "$stage/$SHARE_ASSET.sha256"
  release_bin="$stage/$BINARY_ASSET"
}

# The running script may live inside the share directory the swap replaces, so it is
# staged first; a checkout install (`--local`) keeps the same code path. Piped in
# (`curl ... | bash`) there is no file to copy: the install works but is not
# self-updating, and the wrapper is left out rather than pointed at nothing.
stage_script=""
if [ -f "$script" ]; then
  stage_script="$stage/install.sh"
  cp -f "$script" "$stage_script"
  chmod 0755 "$stage_script"
else
  printf 'p1 install: no script file to copy — install.sh will not be installed\n' >&2
fi

local_bin=""
release_bin=""
case "$mode" in
  latest | release)
    install_release
    stage_share "$stage/$SHARE_ASSET"
    install_binary "$release_bin"
    ;;
  local)
    install_local
    stage_share "$stage/$SHARE_ASSET"
    install_binary "$local_bin"
    ;;
esac

swap_share
if [ -n "$stage_script" ]; then
  install_self
  install_update_wrapper
fi

printf 'p1 install: installed %s\n' "$prefix/bin/p1"
# What is installed, spoken by the binary itself. `p1 login --list` only reads its own
# store; this script never does.
if ! "$prefix/bin/p1" --version; then
  printf 'p1 install: the installed binary did not answer --version\n' >&2
fi
"$prefix/bin/p1" login --list || printf 'p1 install: p1 login --list exited non-zero\n' >&2

case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *)
    printf 'p1 install: %s is not on PATH — add it, e.g. export PATH="%s:\044PATH"\n' \
      "$prefix/bin" "$prefix/bin" >&2
    ;;
esac
