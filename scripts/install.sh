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
#   <prefix>/share/p1/modules                       the released module set (optional)
#   <prefix>/share/p1/install.sh                    this script, so updates need no checkout
#   <prefix>/bin/p1-update                          runs install.sh --latest --prefix <prefix>
#
# The share archive has four roots: environments/, routes/ and profiles/, plus the optional
# modules/ set (manifest.json and packages/). modules/ is optional, so releases that predate
# the WebAssembly migration still install.
#
# Nothing is written to the prefix before the archive is known good. The member listing and
# the tarfile data filter, the three required roots and the module package set with its
# manifest are all checked inside a temporary stage; only then are bin/ and share/ created,
# the .new copies written and the existing rename transaction run. The binary, updater and
# share are committed together with all previous files retained until every rename succeeds;
# an error rolls the previous installation back into place. The module set lives inside the
# share directory, so it is replaced by the same rollback transaction as the shipped data.
#
# `--local` builds the share archive from this checkout's environments, routes and profiles
# only: the local module build arrives with a later slice, so a local install ships no
# share/p1/modules and is checked exactly like a release that has no modules/.
#
# Nothing under ${XDG_CONFIG_HOME:-$HOME/.config}/p1 — p1's credential store and the
# user's own overrides — is read, written or deleted by this script.
#
# Needs bash, `curl` or `gh`, and python3 for the archive and prefix checks: python3 3.12,
# or 3.8.17 / 3.9.17 / 3.10.12 / 3.11.4+ with the tarfile `filter=` security backports.
#
# Environment:
#   P1_REPO             the GitHub repository (default 5omeOtherGuy/phaseone)
#   P1_RELEASE_BASE_URL base for the curl fallback (default the repo's releases page)
#   CARGO_TARGET_DIR    --local's absolute target directory when set
#   CARGO_BUILD_JOBS    --local's requested job count (must not exceed 2)
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

Environment: P1_REPO, P1_RELEASE_BASE_URL, CARGO_TARGET_DIR, CARGO_BUILD_JOBS.
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
# The verified share tree, binary and updater wait in the stage; the prefix paths below are
# set only by publish_staged, once nothing can still refuse the archive.
staged_share=""
staged_bin=""
staged_update=""
bin_new=""
update_new=""
share_new=""
# The sha256 of the verified binary asset, for the manifest's native.sha256 check. Empty for
# --local, which has no release manifest.
binary_sha=""
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

# The optional fourth root: modules/manifest.json beside the modules/packages/ tree. Every
# check runs against the extracted stage — layout, normalised and unique package paths, each
# file's digest and size, no compiled-cache blob — plus, for a release install, the manifest
# pinned to the verified binary and, for --from-release TAG, to TAG. A refusal names the
# archive entry and leaves the prefix untouched.
verify_modules() {
  local root="$1" tarball="$2"
  python3 - "$root" "${binary_sha:-}" "$tag" <<'PY' || die "$(basename "$tarball") has an invalid module package set — nothing installed"
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
binary_sha, tag = sys.argv[2], sys.argv[3]

FORMAT = "p1-release-manifest/1"
TOP_LEVEL = {"format", "commit", "tag", "native", "toolchain", "runtime", "wit",
             "schemas", "packages", "components", "environment_locks"}
TOOLCHAIN_KEYS = {"rustc", "cargo", "wasm_target", "wasm_tools", "wit_bindgen"}
RUNTIME_KEYS = {"wasmtime", "wasmtime_features"}
PACKAGE_KEYS = {"path", "sha256", "size"}


def type_name(value):
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, str):
        return "string"
    if isinstance(value, int):
        return "number"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return type(value).__name__


JSON_NAMES = {str: "string", dict: "object", list: "array", int: "number",
              bool: "boolean", type(None): "null"}


def bad_path(rel):
    """Why rel is not a normalised, relative POSIX path under packages/, else ''."""
    if "\\" in rel:
        return "a backslash"
    if not rel.startswith("packages/"):
        return "no packages/ prefix"
    for part in rel.split("/"):
        if part in ("", ".", ".."):
            return "an empty, '.' or '..' component"
    return ""


modules = root / "modules"
if not (modules.exists() or modules.is_symlink()):
    sys.exit(0)

if modules.is_symlink() or not modules.is_dir():
    raise SystemExit("modules/ is not a directory")
for entry in sorted(modules.iterdir()):
    if entry.name == "manifest.json":
        if entry.is_symlink() or not entry.is_file():
            raise SystemExit("modules/manifest.json is not a regular file")
    elif entry.name == "packages":
        if entry.is_symlink() or not entry.is_dir():
            raise SystemExit("modules/packages is not a directory")
    else:
        raise SystemExit(f"modules/{entry.name}: only manifest.json and packages/ are allowed")

manifest_path = modules / "manifest.json"
if manifest_path.is_symlink() or not manifest_path.is_file():
    raise SystemExit("modules/ has no regular manifest.json")

try:
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"modules/manifest.json is not valid UTF-8 JSON: {error}")

if not isinstance(manifest, dict):
    raise SystemExit(f"modules/manifest.json is not a JSON object but {type_name(manifest)}")
keys = set(manifest)
if keys != TOP_LEVEL:
    detail = []
    if TOP_LEVEL - keys:
        detail.append("missing " + ", ".join(sorted(TOP_LEVEL - keys)))
    if keys - TOP_LEVEL:
        detail.append("unexpected " + ", ".join(sorted(keys - TOP_LEVEL)))
    raise SystemExit("modules/manifest.json has the wrong top-level keys: " + "; ".join(detail))

wanted = {
    "format": (str,), "commit": (str,), "tag": (str, type(None)),
    "native": (dict,), "toolchain": (dict,), "runtime": (dict,),
    "wit": (list,), "schemas": (list,), "packages": (list,),
    "components": (list,), "environment_locks": (list,),
}
for key, kinds in wanted.items():
    value = manifest[key]
    if isinstance(value, bool) or not isinstance(value, kinds):
        names = " or ".join(JSON_NAMES.get(kind, kind.__name__) for kind in kinds)
        raise SystemExit(f"modules/manifest.json: {key} is {type_name(value)}, not {names}")

if manifest["format"] != FORMAT:
    raise SystemExit(f"modules/manifest.json: format is {manifest['format']!r}, not {FORMAT!r}")

commit = manifest["commit"]
if len(commit) != 40 or any(char not in "0123456789abcdef" for char in commit):
    raise SystemExit(f"modules/manifest.json: commit {commit!r} is not a 40-character lowercase hex commit")

native = manifest["native"]
if set(native) != {"asset", "sha256"}:
    raise SystemExit("modules/manifest.json: native must have exactly asset and sha256")
if not isinstance(native["asset"], str) or not isinstance(native["sha256"], str):
    raise SystemExit("modules/manifest.json: native.asset and native.sha256 must be strings")

for section, section_keys in (("toolchain", TOOLCHAIN_KEYS), ("runtime", RUNTIME_KEYS)):
    value = manifest[section]
    if set(value) != section_keys:
        raise SystemExit(f"modules/manifest.json: {section} must have exactly {', '.join(sorted(section_keys))}")
    for name in sorted(section_keys):
        if value[name] is not None and not isinstance(value[name], str):
            raise SystemExit(f"modules/manifest.json: {section}.{name} is {type_name(value[name])}, not string or null")

listed = {}
for index, entry in enumerate(manifest["packages"]):
    where = f"packages[{index}]"
    if not isinstance(entry, dict) or isinstance(entry, bool):
        raise SystemExit(f"modules/manifest.json: {where} is {type_name(entry)}, not an object")
    if set(entry) != PACKAGE_KEYS:
        raise SystemExit(f"modules/manifest.json: {where} must have exactly path, sha256 and size")
    rel = entry["path"]
    if not isinstance(rel, str):
        raise SystemExit(f"modules/manifest.json: {where}.path is {type_name(rel)}, not a string")
    complaint = bad_path(rel)
    if complaint:
        raise SystemExit(f"modules/manifest.json: {where}.path {rel!r} has {complaint}")
    if rel.endswith(".cwasm"):
        raise SystemExit(f"modules/manifest.json: {where}.path {rel!r} is a compiled-cache blob (.cwasm is never shipped)")
    if rel in listed:
        raise SystemExit(f"modules/manifest.json: {where}.path {rel!r} duplicates packages[{listed[rel]}]")
    listed[rel] = index
    if not isinstance(entry["sha256"], str):
        raise SystemExit(f"modules/manifest.json: {where}.sha256 is {type_name(entry['sha256'])}, not a string")
    size = entry["size"]
    if isinstance(size, bool) or not isinstance(size, int) or size < 0:
        raise SystemExit(f"modules/manifest.json: {where}.size is not a byte count")

packages_dir = modules / "packages"
packaged = {}
if packages_dir.exists() or packages_dir.is_symlink():
    if packages_dir.is_symlink() or not packages_dir.is_dir():
        raise SystemExit("modules/packages is not a directory")
    for path in packages_dir.rglob("*"):
        rel = path.relative_to(modules).as_posix()
        if path.is_symlink():
            raise SystemExit(f"modules/{rel}: a symlink is not allowed under modules/")
        if path.is_dir():
            continue
        if not path.is_file():
            raise SystemExit(f"modules/{rel}: a special file is not allowed under modules/")
        if rel.endswith(".cwasm"):
            raise SystemExit(f"modules/{rel}: a compiled-cache blob (.cwasm) is never shipped")
        packaged[rel] = path

if set(packaged) != set(listed):
    detail = []
    missing = sorted(set(listed) - set(packaged))
    extra = sorted(set(packaged) - set(listed))
    if missing:
        detail.append("missing from the archive: " + ", ".join(missing))
    if extra:
        detail.append("not listed in the manifest: " + ", ".join(extra))
    raise SystemExit("modules/manifest.json does not match the packaged files: " + "; ".join(detail))

for entry in manifest["packages"]:
    data = packaged[entry["path"]].read_bytes()
    if len(data) != entry["size"]:
        raise SystemExit(f"modules/{entry['path']}: size is {len(data)} bytes, the manifest says {entry['size']}")
    got = hashlib.sha256(data).hexdigest()
    if got != entry["sha256"]:
        raise SystemExit(f"modules/{entry['path']}: sha256 is {got}, the manifest says {entry['sha256']}")

if binary_sha and native["sha256"] != binary_sha:
    raise SystemExit(f"modules/manifest.json: native.sha256 {native['sha256']!r} does not match the verified binary {binary_sha}")
if tag and manifest["tag"] is not None and manifest["tag"] != tag:
    raise SystemExit(f"modules/manifest.json: tag {manifest['tag']!r} does not match the requested release {tag!r}")
PY
}

# Listing validation and Python's data extraction filter both reject traversal, links and
# special members. Only regular files and directories beneath the four shipped roots are
# accepted, and the result is checked again after extraction. Everything lands in the
# temporary stage, so a rejected archive never reaches the prefix.
stage_share() {
  local tarball="$1"
  staged_share="$stage/share"
  rm -rf "$staged_share"
  mkdir -p "$staged_share"
  python3 - "$tarball" "$staged_share" <<'PY' || die "unsafe or invalid $(basename "$tarball") — nothing installed"
import pathlib
import sys
import tarfile

archive_path, destination = sys.argv[1:]
allowed = {"environments", "routes", "profiles", "modules"}
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
    if [ ! -d "$staged_share/$dir" ] || [ -L "$staged_share/$dir" ]; then
      die "$(basename "$tarball") has no regular $dir/ directory — nothing installed"
    fi
  done
  python3 - "$staged_share" <<'PY' || die "archive produced an unexpected filesystem entry — nothing installed"
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
for path in root.rglob("*"):
    if path.is_symlink() or not (path.is_dir() or path.is_file()):
        raise SystemExit(f"unexpected archive entry: {path}")
PY
  verify_modules "$staged_share" "$tarball"
}

# The verified binary waits in the stage too; publish_staged copies it into the prefix only
# once the whole archive has passed.
stage_binary() {
  local src="$1"
  staged_bin="$stage/p1"
  cp -f "$src" "$staged_bin"
  chmod 0755 "$staged_bin"
}

# The updater is written into the staged share, rather than after its directory is
# committed, so a copy or chmod failure cannot leave a binary without a working updater.
stage_self_and_updater() {
  [ -n "$stage_script" ] || return 0
  cp -f "$stage_script" "$staged_share/install.sh"
  chmod 0755 "$staged_share/install.sh"
  staged_update="$stage/p1-update"
  cat >"$staged_update" <<EOF
#!/usr/bin/env bash
# Written by p1's install.sh (ADR-0065): update p1 to the latest release.
set -euo pipefail
exec "$prefix/share/p1/install.sh" --latest --prefix "$prefix" "\$@"
EOF
  chmod 0755 "$staged_update"
}

# Only now, with the archive and the binary verified, is the prefix created and filled with
# .new copies in their destination directories, so the commit stays a set of same-filesystem
# renames and every earlier refusal leaves the prefix exactly as it was.
publish_staged() {
  mkdir -p "$prefix/bin" "$prefix/share"
  share_new="$prefix/share/.p1.new.$$"
  rm -rf "$share_new"
  mkdir -p "$share_new"
  cp -a "$staged_share/." "$share_new/"
  bin_new="$prefix/bin/.p1.new.$$"
  cp -f "$staged_bin" "$bin_new"
  chmod 0755 "$bin_new"
  if [ -n "$staged_update" ]; then
    update_new="$prefix/bin/.p1-update.new.$$"
    cp -f "$staged_update" "$update_new"
    chmod 0755 "$update_new"
  fi
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
    case "$CARGO_TARGET_DIR" in
      /*) printf '%s\n' "$CARGO_TARGET_DIR" ;;
      *) local_refuse "--local CARGO_TARGET_DIR must be an absolute path: $CARGO_TARGET_DIR" ;;
    esac
  else
    [ -n "${HOME:-}" ] || local_refuse "--local needs HOME to select ~/.cache/cargo-target/p1-release"
    printf '%s/.cache/cargo-target/p1-release\n' "$HOME"
  fi
}

check_local_target() {
  local target=$1 root target_root probe parent filesystem df_output free
  root="$HOME/.cache/cargo-target"
  target_root=$(realpath -m -- "$root") || local_refuse "--local cannot resolve $root"
  target=$(realpath -m -- "$target") || local_refuse "--local cannot resolve target: $target"
  case "$target" in
    "$target_root"/*) ;;
    *) local_refuse "--local target must resolve below $root: $target" ;;
  esac
  if [ -e "$target" ] && [ ! -d "$target" ]; then
    local_refuse "--local target is not a directory: $target"
  fi

  probe=$target
  while [ ! -d "$probe" ]; do
    if [ -e "$probe" ] || [ -L "$probe" ]; then
      local_refuse "--local target ancestor is not a directory: $probe"
    fi
    parent=${probe%/*}
    [ -n "$parent" ] || parent=/
    [ "$parent" != "$probe" ] || local_refuse "--local cannot find an existing target ancestor"
    probe=$parent
  done

  filesystem=$(findmnt -n -o FSTYPE --target "$probe" 2>/dev/null) ||
    local_refuse "--local cannot inspect target filesystem: $target"
  [ "$filesystem" = ext4 ] ||
    local_refuse "--local target is not on an ext4 filesystem: $target (found ${filesystem:-unparseable})"

  df_output=$(df -P --block-size=1 "$probe" 2>/dev/null) ||
    local_refuse "--local cannot inspect target free space: $target"
  if ! free=$(printf '%s\n' "$df_output" | awk 'NR == 2 && NF >= 4 && $4 ~ /^[0-9]+$/ { print $4 }' 2>/dev/null); then
    local_refuse "--local cannot parse target free space: $target"
  fi
  case "$free" in
    '' | *[!0-9]*) local_refuse "--local cannot read target free space: $target" ;;
  esac
  if [ "$free" -lt 12884901888 ]; then
    local_refuse "--local target has $free bytes free, below the 12884901888-byte admission threshold: $target"
  fi
  printf '%s\n' "$target"
}

install_local() {
  local target jobs wrapper
  [ -f "$repo_root/crates/p1-host/Cargo.toml" ] ||
    die "--local needs a checkout: run scripts/install.sh from the repository"
  jobs=${CARGO_BUILD_JOBS:-2}
  case "$jobs" in
    '' | *[!0-9]*) local_refuse "--local CARGO_BUILD_JOBS must be an integer no greater than 2: $jobs" ;;
  esac
  [ "$jobs" -le 2 ] ||
    local_refuse "--local CARGO_BUILD_JOBS must not exceed 2: $jobs"
  wrapper="$repo_root/scripts/rustc-serial"
  [ -x "$wrapper" ] || local_refuse "--local rustc wrapper is not executable: $wrapper"
  target="$(local_target_dir)"
  target="$(check_local_target "$target")"
  export CARGO_TARGET_DIR="$target"
  export CARGO_BUILD_JOBS=2
  export RUSTC_WRAPPER="$wrapper"
  printf 'p1 install: cargo build --release --locked -p p1-host (target %s, jobs %s)\n' \
    "$target" "$CARGO_BUILD_JOBS"
  (cd "$repo_root" && cargo build --release --locked -p p1-host)
  local_bin="$target/release/p1"
  [ -x "$local_bin" ] || die "cargo did not produce $local_bin"
  # The share data come from this checkout: environments, routes and profiles only. The
  # local module build arrives with a later slice, so a --local install ships no modules/.
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
  # The manifest's native.sha256 pins the released binary; compute it from the verified file.
  binary_sha="$(sha256_of "$release_bin")"
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
    printf '%s\n' "${tag:-latest}" >"$staged_share/.p1-release"
    stage_binary "$release_bin"
    stage_self_and_updater
    ;;
  local)
    install_local
    stage_share "$stage/$SHARE_ASSET"
    printf 'local\n' >"$staged_share/.p1-release"
    stage_binary "$local_bin"
    stage_self_and_updater
    ;;
esac

publish_staged
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
