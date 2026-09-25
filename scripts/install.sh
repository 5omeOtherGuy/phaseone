#!/usr/bin/env bash
# Install or update p1 from its public release channel (ADR-0065).
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
# Needs bash, `curl` or `gh`, and python3 for the archive and prefix checks: python3 3.12,
# or 3.8.17 / 3.9.17 / 3.10.12 / 3.11.4+ with the tarfile `filter=` security backports.
#
# Environment:
#   P1_REPO                    the GitHub repository (default 5omeOtherGuy/phaseone)
#   P1_RELEASE_BASE_URL        base for the curl fallback (default the repo's releases page)
#   CARGO_TARGET_DIR           --local's target directory when set
#   CARGO_BUILD_JOBS           --local's job count (default 2)
#   P1_INSTALL_MIN_FREE_BYTES  --local SSD admission threshold (default 12 GiB)
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

Environment: P1_REPO, P1_RELEASE_BASE_URL, CARGO_TARGET_DIR, CARGO_BUILD_JOBS,
P1_INSTALL_MIN_FREE_BYTES.
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

# Python validates and extracts the share archive with the PEP 706 `filter=` kwarg and
# runs the prefix guard below, so probe it once before any install work: a missing or
# older interpreter must fail naming itself, not as a false statement about the archive
# or the prefix.
PYTHON3_NEEDED="python3 3.12, or 3.8.17 / 3.9.17 / 3.10.12 / 3.11.4+ (needs the tarfile filter= backport)"
command -v python3 >/dev/null 2>&1 ||
  die "$PYTHON3_NEEDED is required, but there is no python3 on PATH"
python3 - <<'PY' || die "$PYTHON3_NEEDED is required, but $(command -v python3) ($(python3 -V 2>&1)) rejected tarfile's data filter"
import io
import tarfile

# The exact call the installer makes (an empty in-memory archive: no filesystem writes).
buffer = io.BytesIO()
with tarfile.open(fileobj=buffer, mode="w"):
    pass
buffer.seek(0)
with tarfile.open(fileobj=buffer, mode="r") as archive:
    archive.extractall(path=".", members=[], filter="data")
PY

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
# One fixed backup slot per prefix, so a failed install leaves at most one rollback copy
# behind (replaced by the next install, and named in the failure message) instead of one
# per attempt.
bin_prev="$prefix/bin/.p1.previous"
update_prev="$prefix/bin/.p1-update.previous"
share_prev="$prefix/share/.p1.previous"
transaction=0
new_started=0

rollback_install() {
  local restore_error=0 restore_from restore_to index
  [ "$transaction" -eq 1 ] || return 0
  if [ "$new_started" -eq 1 ]; then
    rm -rf "$prefix/bin/p1" "$prefix/bin/p1-update" "$prefix/share/p1"
  fi
  restore_from=("$bin_prev" "$update_prev" "$share_prev")
  restore_to=("$prefix/bin/p1" "$prefix/bin/p1-update" "$prefix/share/p1")
  for index in "${!restore_from[@]}"; do
    if [ -e "${restore_from[$index]}" ] || [ -L "${restore_from[$index]}" ]; then
      mv "${restore_from[$index]}" "${restore_to[$index]}" || restore_error=1
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
# A signal must end the script after the rollback, not let it resume a half-swapped
# install: the INT/TERM handler runs cleanup and then exits.
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

# Keep the previous binary, updater and share together; a restore that fails is never
# swallowed, so name what is left and let the caller die.
restore_previous_install() {
  if ! rollback_install; then
    printf 'p1 install: the previous install could not be restored — leftovers: %s %s %s\n' \
      "$bin_prev" "$update_prev" "$share_prev" >&2
  fi
}

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
  local asset="$1" fetched=0
  if command -v gh >/dev/null 2>&1; then
    printf 'p1 install: gh release download %s\n' "$asset"
    set -- --repo "$repo" --dir "$stage" --pattern "$asset" --clobber
    if [ -n "$tag" ]; then
      set -- "$tag" "$@"
    fi
    if gh release download "$@"; then
      fetched=1
    fi
  fi
  if [ "$fetched" -eq 0 ]; then
    # gh is an optimization, not an authentication requirement: a failure falls back to
    # the public, credential-free release URL, and whatever a failed gh left behind (a
    # truncated asset, most likely) is discarded first.
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
# Written by p1's install.sh (ADR-0065): update p1 to the latest release.
set -euo pipefail
exec "$prefix/share/p1/install.sh" --latest --prefix "$prefix" "\$@"
EOF
  chmod 0755 "$wrapper"
  update_new="$wrapper"
}

# --local: a cloud build is normal. This fallback uses one per-task target on the
# ext4 SSD, with the configured free-space admission check; the repository's own
# target/ is never used.
local_refuse() {
  printf 'p1 install: %s\n' "$*" >&2
  exit 2
}

local_target_dir() {
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    printf '%s\n' "$CARGO_TARGET_DIR"
  else
    [ -n "${HOME:-}" ] || local_refuse "--local needs HOME to select ~/.cache/cargo-target/p1-release"
    printf '%s/.cache/cargo-target/p1-release\n' "$HOME"
  fi
}

check_local_target() {
  local target=$1 root target_root probe parent filesystem min_free df_output free
  root="$HOME/.cache/cargo-target"
  target_root=$(realpath -m -- "$root") || local_refuse "--local cannot resolve $root"
  target=$(realpath -m -- "$target") || local_refuse "--local cannot resolve target: $target"
  case "$target" in
    "$target_root"/*) ;;
    *) local_refuse "--local target must resolve below $root: $target" ;;
  esac
  case "$target" in
    /*) ;;
    *) local_refuse "--local target must be an absolute path: $target" ;;
  esac
  if [ -e "$target" ] && [ ! -d "$target" ]; then
    local_refuse "--local target is not a directory: $target"
  fi

  probe=$target
  while [ ! -e "$probe" ]; do
    parent=${probe%/*}
    [ -n "$parent" ] || parent=/
    [ "$parent" != "$probe" ] || break
    probe=$parent
  done
  [ -d "$probe" ] || local_refuse "--local target has no directory filesystem: $target"

  filesystem=$(stat -f -c %T "$probe" 2>/dev/null) ||
    local_refuse "--local cannot inspect target filesystem: $target"
  case "$filesystem" in
    ext2 | ext3 | ext2/ext3) ;;
    *) local_refuse "--local target is not on an ext4 filesystem: $target (found $filesystem)" ;;
  esac

  min_free=${P1_INSTALL_MIN_FREE_BYTES:-12884901888}
  case "$min_free" in
    '' | *[!0-9]*) local_refuse "P1_INSTALL_MIN_FREE_BYTES must be a non-negative integer" ;;
  esac
  df_output=$(df -P --block-size=1 "$probe" 2>/dev/null) ||
    local_refuse "--local cannot inspect target free space: $target"
  free=$(printf '%s\n' "$df_output" | awk 'NR == 2 { print $4 }')
  case "$free" in
    '' | *[!0-9]*) local_refuse "--local cannot read target free space: $target" ;;
  esac
  if [ "$free" -lt "$min_free" ]; then
    local_refuse "--local target has $free bytes free, below the $min_free-byte admission threshold: $target"
  fi
}

install_local() {
  local target
  [ -f "$repo_root/crates/p1-host/Cargo.toml" ] ||
    die "--local needs a checkout: run scripts/install.sh from the repository"
  target="$(local_target_dir)"
  check_local_target "$target"
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

  # A stale fixed slot may be the only manual-recovery copy after a failed rollback. Move
  # it aside rather than deleting it, and discard both it and this install's retained
  # previous files only after every new-file rename has succeeded.
  if [ "$had_binary" -eq 1 ]; then
    if [ -e "$bin_prev" ] || [ -L "$bin_prev" ]; then
      mv "$bin_prev" "$bin_prev.stale" || {
        restore_previous_install
        die "could not preserve the previous binary backup"
      }
    fi
    if ! mv "$prefix/bin/p1" "$bin_prev"; then
      restore_previous_install
      die "could not retain the previous binary"
    fi
  fi
  if [ "$had_updater" -eq 1 ]; then
    if [ -e "$update_prev" ] || [ -L "$update_prev" ]; then
      mv "$update_prev" "$update_prev.stale" || {
        restore_previous_install
        die "could not preserve the previous updater backup"
      }
    fi
    if ! mv "$prefix/bin/p1-update" "$update_prev"; then
      restore_previous_install
      die "could not retain the previous updater"
    fi
  fi
  if [ "$had_share" -eq 1 ]; then
    if [ -e "$share_prev" ] || [ -L "$share_prev" ]; then
      mv "$share_prev" "$share_prev.stale" || {
        restore_previous_install
        die "could not preserve the previous share data backup"
      }
    fi
    if ! mv "$prefix/share/p1" "$share_prev"; then
      restore_previous_install
      die "could not retain the previous share data"
    fi
  fi

  new_started=1
  if ! mv "$share_new" "$prefix/share/p1" || ! mv "$bin_new" "$prefix/bin/p1"; then
    restore_previous_install
    die "could not commit the new share data and binary"
  fi
  share_new=""
  bin_new=""
  if [ -n "$update_new" ]; then
    if ! mv "$update_new" "$prefix/bin/p1-update"; then
      restore_previous_install
      die "could not commit the new updater"
    fi
    update_new=""
  fi

  rm -f "$bin_prev" "$update_prev"
  rm -rf "$share_prev"
  rm -f "$bin_prev.stale" "$update_prev.stale"
  rm -rf "$share_prev.stale"
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
# `p1 --version` prints `p1 <version> (<short sha> <date>)`, so the sha is the
# parenthesised hex token — `unknown` (a build without git) is not hex and yields nothing.
release_sha() {
  local binary="$1" version
  version="$("$binary" --version 2>/dev/null)" || return 1
  printf '%s\n' "$version" |
    awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\([0-9a-f]+$/) { print substr($i, 2); exit } }'
}

# The tag the "latest" release carries. Resolving it needs one cheap request, not four
# downloads, so an up-to-date install can say so without fetching and discarding the
# assets: gh answers from the API, and the public releases/latest redirect (no
# credential, just curl) carries the tag in its final URL.
latest_tag() {
  local effective
  if command -v gh >/dev/null 2>&1; then
    gh release view --repo "$repo" --json tagName --jq .tagName 2>/dev/null && return 0
  fi
  effective="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$base/latest" 2>/dev/null)" || return 1
  case "$effective" in
    */tag/*) printf '%s\n' "${effective##*/tag/}" ;;
    *) return 1 ;;
  esac
}

already_installed() {
  local want_tag want_sha installed_sha
  [ -z "$force" ] || return 1
  [ -x "$prefix/bin/p1" ] || return 1
  if [ "$mode" = release ]; then
    want_tag="$tag"
  else
    want_tag="$(latest_tag)" || return 1
    [ -n "$want_tag" ] || return 1
  fi
  if [ -f "$prefix/share/p1/.p1-release" ] &&
     [ "$(cat "$prefix/share/p1/.p1-release")" = "$want_tag" ]; then
    return 0
  fi
  case "$want_tag" in
    main-?*) ;;
    *) return 1 ;;
  esac
  want_sha="${want_tag#main-}"
  installed_sha="$(release_sha "$prefix/bin/p1")" || return 1
  [ "$installed_sha" = "$want_sha" ]
}

local_bin=""
release_bin=""
case "$mode" in
  latest | release)
    if already_installed; then
      printf 'p1 install: release %s is already installed at %s (use --force to reinstall)\n' \
        "${tag:-latest}" "$prefix"
      exit 0
    fi
    install_release
    # When the tag could not be resolved up front, the downloaded binary itself answers
    # whether this release is the installed one; the prefix is still untouched here.
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
