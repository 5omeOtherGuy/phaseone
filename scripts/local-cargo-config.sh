#!/usr/bin/env bash
# Machine-local Cargo configuration for this checkout/worktree (untracked; CI is unaffected).
#
# Storage policy: every NEW Cargo target and all large build artifacts stay below
# /mnt/build/cargo-target on the ext4 filesystem mounted at /mnt/build. This machine
# provides that filesystem as the internal-HDD build image. Defaults are isolated
# per canonical checkout path (D20), so worktrees cannot reuse each other's
# workspace artifacts. This script never copies, moves, removes, or changes an old
# target tree.
#
# CARGO_TARGET_DIR is a caller-owned, per-task override. Do not reuse an explicit
# target across worktrees: sharing Cargo's target tree can recreate the D20 incident.
#
# Usage: scripts/local-cargo-config.sh [--dry-run] [checkout-dir]
set -euo pipefail

die() {
  printf 'local-cargo-config: %s\n' "$*" >&2
  exit 1
}

usage() {
  printf 'Usage: %s [--dry-run] [checkout-dir]\n' "${0##*/}" >&2
}

if (( $# > 2 )); then
  usage
  exit 2
fi
if [[ ${1-} == --dry-run ]]; then
  dry_run=true
  shift
else
  dry_run=false
fi
if (( $# > 1 )); then
  usage
  exit 2
fi

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
checkout_input=${1-"$here"}
checkout="$(realpath -- "$checkout_input")" || die "cannot resolve checkout: $checkout_input"
[[ -d $checkout ]] || die "checkout is not a directory: $checkout"
checkout_name=${checkout##*/}
[[ -n $checkout_name ]] || die "cannot determine checkout basename"

build_root=/mnt/build
target_root=/mnt/build/cargo-target
if [[ ${CARGO_TARGET_DIR+x} ]]; then
  [[ -n $CARGO_TARGET_DIR ]] || die 'CARGO_TARGET_DIR must not be empty'
  [[ $CARGO_TARGET_DIR == /* ]] || die 'CARGO_TARGET_DIR must be absolute'
  target_candidate=$CARGO_TARGET_DIR
else
  checkout_hash="$(printf '%s' "$checkout" | sha256sum)" || die 'cannot hash checkout path'
  checkout_hash=${checkout_hash%% *}
  target_candidate="$target_root/$checkout_name-${checkout_hash:0:12}"
fi

target="$(realpath -m -- "$target_candidate")" || die "cannot resolve target: $target_candidate"
[[ $target != "$target_root" ]] || die "target must be below $target_root, not $target_root itself"
case $target in
  "$target_root"/*) ;;
  *) die "target resolves outside $target_root: $target" ;;
esac
if [[ -e $target && ! -d $target ]]; then
  die "target exists but is not a directory: $target"
fi

require_build_ext4() {
  local checked_path=$1 mount_output line
  local found=false

  if ! mount_output="$(findmnt -rn -T "$checked_path" -o TARGET,FSTYPE)"; then
    die "cannot inspect mount for $checked_path"
  fi
  while IFS= read -r line; do
    if [[ $line == '/mnt/build ext4' ]]; then
      found=true
      break
    fi
  done <<< "$mount_output"
  if [[ $found != true ]]; then
    die "$checked_path is not on the required /mnt/build ext4 filesystem"
  fi
}

# A reported autofs line is allowed, but there must also be the exact ext4 entry.
require_build_ext4 "$build_root/."

ancestor=$target
while [[ ! -d $ancestor ]]; do
  if [[ -e $ancestor || -L $ancestor ]]; then
    die "target ancestor exists but is not a directory: $ancestor"
  fi
  parent=${ancestor%/*}
  [[ -n $parent ]] || parent=/
  [[ $parent != "$ancestor" ]] || die "cannot find an existing ancestor of $target"
  ancestor=$parent
done
# This also rejects a nested mount between /mnt/build and the selected target.
require_build_ext4 "$ancestor"

worktree_listing="$(git -C "$checkout" worktree list --porcelain)" || \
  die "cannot list worktrees for $checkout"
main_checkout=
while IFS= read -r line; do
  if [[ $line == 'worktree '* ]]; then
    main_checkout=${line#worktree }
    break
  fi
done <<< "$worktree_listing"
[[ -n $main_checkout ]] || die "git returned no main worktree for $checkout"

# Keep a wrapper inside the caller checkout when present; otherwise use main.
wrapper="$main_checkout/scripts/rustc-serial"
if [[ -x $checkout/scripts/rustc-serial ]]; then
  wrapper="$checkout/scripts/rustc-serial"
fi
[[ -x $wrapper ]] || die "rustc-serial wrapper is not executable: $wrapper"

config_dir="$checkout/.cargo"
config="$config_dir/config.toml"
if [[ -L $config_dir ]]; then
  die "Cargo config parent must not be a symlink: $config_dir"
fi
if [[ -e $config_dir && ! -d $config_dir ]]; then
  die "Cargo config parent is not a directory: $config_dir"
fi
if [[ -L $config ]]; then
  die "Cargo config must not be a symlink: $config"
fi
if [[ -e $config && ! -f $config ]]; then
  die "Cargo config path is not a regular file: $config"
fi
if [[ -e $config ]]; then
  # Writing through the name would rewrite every file linked to the same inode.
  config_links="$(stat -c %h -- "$config")" || die "cannot inspect Cargo config link count: $config"
  [[ $config_links == 1 ]] || die "Cargo config has multiple hard links: $config"
fi
config_resolved="$(realpath -m -- "$config")" || die "cannot resolve Cargo config path"
expected_config="$checkout/.cargo/config.toml"
[[ $config_resolved == "$expected_config" ]] || \
  die "Cargo config resolves outside the expected path: $config_resolved"

# TOML basic strings require backslashes and double quotes to be escaped.
toml_escape() {
  local value=$1
  case $value in
    *[[:cntrl:]]*) die "path contains a control character" ;;
  esac
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  printf '%s' "$value"
}
target_toml="$(toml_escape "$target")" || exit 1
wrapper_toml="$(toml_escape "$wrapper")" || exit 1

emit_config() {
  printf '%s\n' \
    '# Generated by scripts/local-cargo-config.sh — machine-local, not committed.' \
    "# New targets use the ext4 filesystem mounted at /mnt/build, this machine's internal-HDD build image." \
    '# Per-checkout targets preserve D20 isolation; old target trees are not changed.' \
    '# An explicit CARGO_TARGET_DIR is caller-owned and must not be shared by worktrees.' \
    '[build]' \
    "target-dir = \"$target_toml\"" \
    'jobs = 2' \
    'incremental = false' \
    "rustc-wrapper = \"$wrapper_toml\"" \
    '' \
    '[profile.dev.package."*"]' \
    'debug = false'
}

if [[ $dry_run == true ]]; then
  emit_config
  exit 0
fi

# Unlike validation above, do not allow an absent build root to become writable.
[[ -d $build_root ]] || die "$build_root must already exist; refusing to create it"
mkdir -p -- "$target" "$config_dir"
emit_config > "$config"
printf 'cargo config: %s -> target-dir %s\n' "$config" "$target"
