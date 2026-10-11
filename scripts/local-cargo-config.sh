#!/usr/bin/env bash
# Machine-local Cargo configuration for this checkout/worktree (untracked; CI is unaffected).
#
# Storage policy: every NEW Cargo target is one per checkout/task, on ext4 only, below one of
# two build roots: ~/.cache/cargo-target on the SSD, or the data tier's build root on the
# internal HDD (P1_BUILD_HDD, else FLEET_BUILD_HDD from `fleet-data env`, else /data/build).
# `--tier auto` (the default) picks the HDD root when the SSD has less free space than its build
# admission (P1_BUILD_ADMISSION_GIB, default 12), else the SSD root (owner order 2026-09-29 23:30);
# `--tier ssd` and `--tier hdd` pick one. The tier is decided when the config is written.
# Defaults are isolated per canonical checkout path (D20), so worktrees cannot reuse
# each other's workspace artifacts. This script never copies, moves, removes, or changes an old
# target tree.
#
# CARGO_TARGET_DIR is a caller-owned, per-task override below either root; it wins over the
# tier. Do not reuse an explicit target across worktrees: sharing Cargo's target tree can
# recreate the D20 incident.
#
# Usage: scripts/local-cargo-config.sh [--dry-run] [--tier ssd|hdd|auto] [checkout-dir]
set -euo pipefail

die() {
  printf 'local-cargo-config: %s\n' "$*" >&2
  exit 1
}

usage() {
  printf 'Usage: %s [--dry-run] [--tier ssd|hdd|auto] [checkout-dir]\n' "${0##*/}" >&2
}

dry_run=false
tier=auto
while (( $# > 0 )); do
  case $1 in
    --dry-run) dry_run=true; shift ;;
    --tier)
      (( $# > 1 )) || { usage; exit 2; }
      case $2 in
        ssd|hdd|auto) tier=$2 ;;
        *) usage; exit 2 ;;
      esac
      shift 2
      ;;
    -*) usage; exit 2 ;;
    *) break ;;
  esac
done
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

[[ -n ${HOME-} ]] || die 'HOME must be set, non-empty and absolute'
[[ $HOME == /* ]] || die 'HOME must be set, non-empty and absolute'
target_root="$HOME/.cache/cargo-target"
target_root="$(realpath -m -- "$target_root")" || die "cannot resolve target root: $HOME/.cache/cargo-target"
hdd_root="$(realpath -m -- "${P1_BUILD_HDD:-${FLEET_BUILD_HDD:-/data/build}}")" || \
  die "cannot resolve the HDD build root"

# The nearest existing directory at or above a path, for free-space and mount checks.
existing_ancestor() {
  local path=$1 parent
  while [[ ! -d $path ]]; do
    if [[ -e $path || -L $path ]]; then
      die "ancestor exists but is not a directory: $path"
    fi
    parent=${path%/*}
    [[ -n $parent ]] || parent=/
    [[ $parent != "$path" ]] || die "cannot find an existing ancestor of $1"
    path=$parent
  done
  printf '%s' "$path"
}

if [[ ${CARGO_TARGET_DIR+x} ]]; then
  [[ -n $CARGO_TARGET_DIR ]] || die 'CARGO_TARGET_DIR must not be empty'
  [[ $CARGO_TARGET_DIR == /* ]] || die 'CARGO_TARGET_DIR must be absolute'
  target_candidate=$CARGO_TARGET_DIR
else
  if [[ $tier == auto ]]; then
    admission_gib=${P1_BUILD_ADMISSION_GIB:-12}
    [[ $admission_gib =~ ^[0-9]+$ ]] || die 'P1_BUILD_ADMISSION_GIB must be a whole number of GiB'
    ssd_probe="$(existing_ancestor "$target_root")" || exit 1
    ssd_free="$(df -B1 --output=avail -- "$ssd_probe" | tail -n 1)" || \
      die "cannot read free space at $ssd_probe"
    ssd_free=${ssd_free//[[:space:]]/}
    [[ $ssd_free =~ ^[0-9]+$ ]] || die "cannot read free space at $ssd_probe: $ssd_free"
    if (( ssd_free < admission_gib * 1024 * 1024 * 1024 )); then
      tier=hdd
      printf 'local-cargo-config: %s GiB free on the SSD, below its %s GiB build admission; using %s\n' \
        "$(( ssd_free / 1024 / 1024 / 1024 ))" "$admission_gib" "$hdd_root" >&2
    else
      tier=ssd
    fi
  fi
  tier_root=$target_root
  [[ $tier == hdd ]] && tier_root=$hdd_root
  checkout_hash="$(printf '%s' "$checkout" | sha256sum)" || die 'cannot hash checkout path'
  checkout_hash=${checkout_hash%% *}
  target_candidate="$tier_root/$checkout_name-${checkout_hash:0:12}"
fi

target="$(realpath -m -- "$target_candidate")" || die "cannot resolve target: $target_candidate"
[[ $target != "$target_root" ]] || die "target must be below $target_root, not $target_root itself"
[[ $target != "$hdd_root" ]] || die "target must be below $hdd_root, not $hdd_root itself"
case $target in
  "$target_root"/*) root=$target_root ;;
  "$hdd_root"/*) root=$hdd_root ;;
  *) die "target resolves outside $target_root and $hdd_root: $target" ;;
esac
if [[ -e $target && ! -d $target ]]; then
  die "target exists but is not a directory: $target"
fi

require_ext4() {
  local checked_path=$1 mount_output line filesystem
  local found=false

  if ! mount_output="$(findmnt -rn -T "$checked_path" -o TARGET,FSTYPE)"; then
    die "cannot inspect mount for $checked_path"
  fi
  while IFS= read -r line; do
    filesystem=${line#* }
    filesystem=${filesystem%% *}
    if [[ $filesystem == ext4 ]]; then
      found=true
      break
    fi
  done <<< "$mount_output"
  if [[ $found != true ]]; then
    die "$checked_path is not on an ext4 filesystem; NTFS, USB and other filesystems are refused"
  fi
}

# A reported autofs line is allowed, but there must also be an exact ext4 entry.
root_ancestor=$root
while [[ ! -d $root_ancestor ]]; do
  if [[ -e $root_ancestor || -L $root_ancestor ]]; then
    die "target root ancestor exists but is not a directory: $root_ancestor"
  fi
  root_parent=${root_ancestor%/*}
  [[ -n $root_parent ]] || root_parent=/
  [[ $root_parent != "$root_ancestor" ]] || die "cannot find an existing ancestor of $root"
  root_ancestor=$root_parent
done
require_ext4 "$root_ancestor"

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
# This also rejects a nested mount between the target root and the selected target.
require_ext4 "$ancestor"

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
    '# New targets live below ~/.cache/cargo-target on the SSD (ext4), or below the HDD build root when the SSD is below its 12 GiB build admission (8 GiB floor); build in CI where possible.' \
    '# Per-checkout targets preserve D20 isolation; old target trees are not changed.' \
    '# An explicit CARGO_TARGET_DIR is caller-owned and must not be shared by worktrees.' \
    '[build]' \
    "target-dir = \"$target_toml\"" \
    'jobs = 3' \
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

mkdir -p -- "$target" "$config_dir"
emit_config > "$config"
printf 'cargo config: %s -> target-dir %s\n' "$config" "$target"
