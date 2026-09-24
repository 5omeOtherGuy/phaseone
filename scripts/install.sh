#!/usr/bin/env bash
# Install or update p1 from its public release channel (ADR-0063).
#
#   scripts/install.sh [--latest | --from-release TAG | --local] [--prefix DIR] [--force]
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
# Nothing is written to the prefix before both checksums pass. The binary, updater and
# share are staged first, then committed with all previous files retained until every
# rename succeeds; an error rolls the previous installation back into place.
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
usage: install.sh [--latest | --from-release TAG | --local] [--prefix DIR] [--force]

  --latest              install the latest published release (default)
  --from-release TAG    install the release tagged TAG
  --local               build this checkout in release profile and install it
  --prefix DIR          where to install (default $HOME/.local)
  --force               reinstall even when TAG is already installed

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
force=""

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
    --force) force=1 ;;
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
if [ "$prefix" != "/" ]; then
  prefix="${prefix%/}"
fi

# Check the prospective bin/share paths, following existing symlinks, before any prefix
# write. A custom prefix must never be allowed to target the user's private p1 tree.
config_root="${XDG_CONFIG_HOME:-${HOME:-/nonexistent}/.config}"
python3 - "$config_root/p1" "$prefix/bin" "$prefix/share" <<'PY' || die "refusing a prefix whose bin or share path enters ${config_root}/p1"
import os
import sys

protected = os.path.realpath(sys.argv[1])
for candidate in sys.argv[2:]:
    resolved = os.path.realpath(candidate)
    if resolved == protected or resolved.startswith(protected + os.sep):
        raise SystemExit(1)
PY

stage="$(mktemp -d "${TMPDIR:-/tmp}/p1-install.XXXXXX")"
bin_new=""
update_new=""
share_new=""
bin_prev="$prefix/bin/.p1.previous.$$"
update_prev="$prefix/bin/.p1-update.previous.$$"
share_prev="$prefix/share/.p1.previous.$$"
transaction=0
new_started=0

rollback_install() {
  local restore_error=0 pair
  [ "$transaction" -eq 1 ] || return 0
  if [ "$new_started" -eq 1 ]; then
    rm -rf "$prefix/bin/p1" "$prefix/bin/p1-update" "$prefix/share/p1"
  fi
  for pair in \
    "$bin_prev:$prefix/bin/p1" \
    "$update_prev:$prefix/bin/p1-update" \
    "$share_prev:$prefix/share/p1"; do
    if [ -e "${pair%%:*}" ] || [ -L "${pair%%:*}" ]; then
      mv "${pair%%:*}" "${pair#*:}" || restore_error=1
    fi
  done
  transaction=0
  return "$restore_error"
}

cleanup() {
  if [ "$transaction" -eq 1 ]; then
    rollback_install || printf 'p1 install: rollback could not restore the previous install\n' >&2
  fi
  rm -rf "$stage"
  [ -z "$bin_new" ] || rm -f "$bin_new"
  [ -z "$update_new" ] || rm -f "$update_new"
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

download_with_curl() {
  local asset="$1" url
  if [ -n "$tag" ]; then
    url="$base/download/$tag/$asset"
  else
    url="$base/latest/download/$asset"
  fi
  printf 'p1 install: curl %s\n' "$url"
  curl -fsSL -o "$stage/$asset" "$url" || die "curl could not download $url"
}

download_asset() {
  local asset="$1"
  if command -v gh >/dev/null 2>&1; then
    printf 'p1 install: gh release download %s\n' "$asset"
    if [ -n "$tag" ]; then
      gh release download "$tag" --repo "$repo" --dir "$stage" --pattern "$asset" --clobber || true
    else
      gh release download --repo "$repo" --dir "$stage" --pattern "$asset" --clobber || true
    fi
  fi
  if [ ! -f "$stage/$asset" ]; then
    # gh is an optimization, not an authentication requirement. Discard a partial
    # download before using the public, credential-free release URL.
    rm -f "$stage/$asset"
    download_with_curl "$asset"
  fi
}

# Every install artifact is prepared in its destination directory before the transaction
# starts, so the final commit consists only of same-filesystem renames.
stage_binary() {
  local src="$1"
  mkdir -p "$prefix/bin"
  bin_new="$prefix/bin/.p1.new.$$"
  cp -f "$src" "$bin_new"
  chmod 0755 "$bin_new"
}

# Listing validation and Python's data extraction filter both reject traversal, links and
# special members. Only regular files and directories beneath the three shipped roots are
# accepted, and the resulting layout is checked again after extraction.
stage_share() {
  local tarball="$1"
  mkdir -p "$prefix/share"
  share_new="$prefix/share/.p1.new.$$"
  rm -rf "$share_new"
  mkdir -p "$share_new"
  python3 - "$tarball" "$share_new" <<'PY' || die "unsafe or invalid $(basename "$tarball") — nothing installed"
import pathlib
import sys
import tarfile

archive_path, destination = sys.argv[1:]
allowed = {"environments", "routes", "profiles"}
with tarfile.open(archive_path, "r:gz") as archive:
    members = archive.getmembers()
    for member in members:
        path = pathlib.PurePosixPath(member.name)
        if (not path.parts or path.is_absolute() or ".." in path.parts
                or path.parts[0] not in allowed
                or not (member.isdir() or member.isfile())):
            raise SystemExit(f"unsafe archive member: {member.name!r}")
    archive.extractall(destination, members=members, filter="data")
PY
  for dir in environments routes profiles; do
    if [ ! -d "$share_new/$dir" ] || [ -L "$share_new/$dir" ]; then
      die "$(basename "$tarball") has no regular $dir/ directory — nothing installed"
    fi
  done
  python3 - "$share_new" <<'PY' || die "archive produced an unexpected filesystem entry — nothing installed"
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
for path in root.rglob("*"):
    if path.is_symlink() or not (path.is_dir() or path.is_file()):
        raise SystemExit(f"unexpected archive entry: {path}")
PY
}

# The updater is written into the staged share, rather than after its directory is
# committed, so a copy or chmod failure cannot leave a binary without a working updater.
stage_self_and_updater() {
  local wrapper
  [ -n "$stage_script" ] || return 0
  cp -f "$stage_script" "$share_new/install.sh"
  chmod 0755 "$share_new/install.sh"
  wrapper="$prefix/bin/.p1-update.new.$$"
  cat >"$wrapper" <<EOF
#!/usr/bin/env bash
# Written by p1's install.sh (ADR-0063): update p1 to the latest release.
set -euo pipefail
exec "$prefix/share/p1/install.sh" --latest --prefix "$prefix" "\$@"
EOF
  chmod 0755 "$wrapper"
  update_new="$wrapper"
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

commit_install() {
  local had_binary=0 had_updater=0 had_share=0
  [ ! -e "$prefix/bin/p1" ] || had_binary=1
  [ ! -e "$prefix/bin/p1-update" ] || had_updater=1
  [ ! -e "$prefix/share/p1" ] || had_share=1
  transaction=1

  if [ "$had_binary" -eq 1 ] && ! mv "$prefix/bin/p1" "$bin_prev"; then
    rollback_install || true
    die "could not retain the previous binary"
  fi
  if [ "$had_updater" -eq 1 ] && ! mv "$prefix/bin/p1-update" "$update_prev"; then
    rollback_install || true
    die "could not retain the previous updater"
  fi
  if [ "$had_share" -eq 1 ] && ! mv "$prefix/share/p1" "$share_prev"; then
    rollback_install || true
    die "could not retain the previous share data"
  fi

  new_started=1
  if ! mv "$share_new" "$prefix/share/p1" || ! mv "$bin_new" "$prefix/bin/p1"; then
    rollback_install || true
    die "could not commit the new share data and binary"
  fi
  share_new=""
  bin_new=""
  if [ -n "$update_new" ]; then
    if ! mv "$update_new" "$prefix/bin/p1-update"; then
      rollback_install || true
      die "could not commit the new updater"
    fi
    update_new=""
  fi

  rm -f "$bin_prev" "$update_prev"
  rm -rf "$share_prev"
  transaction=0
}

# The running script may live inside the share directory the swap replaces, so copy it
# into the transaction's staging area first. Piped-in scripts still install, but cannot
# make the result self-updating.
if [ -f "$script" ]; then
  stage_script="$stage/install.sh"
  cp -f "$script" "$stage_script"
  chmod 0755 "$stage_script"
else
  stage_script=""
  printf 'p1 install: no script file to copy — install.sh will not be installed\n' >&2
fi

# Release tags name the commit (main-<sha>), so an already installed main-<sha> is the
# same release. Arbitrary tags are tracked in the share by the installer itself.
release_sha() {
  local binary="$1" version
  version="$($binary --version 2>/dev/null)" || return 1
  printf '%s\n' "$version" | awk '{ if ($1 == "p1" && $3 ~ /^\([0-9a-f]+\)$/) print substr($3, 2, length($3)-2) }'
}

already_installed() {
  local want_sha installed_sha
  [ "$mode" = release ] || return 1
  [ -z "$force" ] || return 1
  if [ -f "$prefix/share/p1/.p1-release" ] &&
     [ "$(cat "$prefix/share/p1/.p1-release")" = "$tag" ]; then
    return 0
  fi
  case "$tag" in
    main-?*) ;;
    *) return 1 ;;
  esac
  want_sha="${tag#main-}"
  [ -x "$prefix/bin/p1" ] || return 1
  installed_sha="$(release_sha "$prefix/bin/p1")" || return 1
  [ "$installed_sha" = "$want_sha" ]
}

local_bin=""
release_bin=""
case "$mode" in
  latest | release)
    if already_installed; then
      printf 'p1 install: release %s is already installed at %s (use --force to reinstall)\n' \
        "$tag" "$prefix"
      exit 0
    fi
    install_release
    if [ "$mode" = latest ] && [ -z "$force" ] && [ -x "$prefix/bin/p1" ]; then
      installed_release_sha="$(release_sha "$prefix/bin/p1" || true)"
      downloaded_release_sha="$(release_sha "$release_bin" || true)"
      if [ -n "$installed_release_sha" ] &&
         [ "$installed_release_sha" = "$downloaded_release_sha" ]; then
        printf 'p1 install: this release is already installed at %s (use --force to reinstall)\n' \
          "$prefix"
        exit 0
      fi
    fi
    stage_share "$stage/$SHARE_ASSET"
    printf '%s\n' "${tag:-latest}" >"$share_new/.p1-release"
    stage_binary "$release_bin"
    stage_self_and_updater
    ;;
  local)
    install_local
    stage_share "$stage/$SHARE_ASSET"
    printf 'local\n' >"$share_new/.p1-release"
    stage_binary "$local_bin"
    stage_self_and_updater
    ;;
esac

commit_install

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
