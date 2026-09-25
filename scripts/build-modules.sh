#!/usr/bin/env bash
# Builds the module packages under modules/: each package's component is built with the pinned
# target, validated, and published with the extracted world, its digest, its import list and
# its manifest under modules/target/p1-modules/<package>/. The package format and the fields
# this script checks are frozen in docs/design/modules/package.md.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
usage: scripts/build-modules.sh [--all | --package <name>]
       scripts/build-modules.sh --help

--all             build every module package under modules/ (the default)
--package <name>  build only the package modules/<name>/
A module package is a directory modules/p1-module-*/ whose Cargo.toml carries a
[package.metadata.p1-module] table; bindings crates (p1-bindings-*) are not packages.
Exit 0 when every package builds, 1 when one fails, 2 on a usage error.
EOF
}

# Prints the name of every module package, sorted. A package is a directory modules/p1-module-*/
# whose manifest declares the frozen metadata table: modules/wit/ holds the WIT worlds,
# modules/Cargo.toml is the workspace root and a bindings crate is a library, so none is one.
module_packages() {
  local dir name
  [ -d modules ] || return 0
  for dir in modules/p1-module-*/; do
    [ -d "$dir" ] || continue
    name="$(basename "$dir")"
    [ -f "$dir/Cargo.toml" ] || continue
    grep -q '^\[package\.metadata\.p1-module\]' "$dir/Cargo.toml" && echo "$name"
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

fail() {
  echo "build-modules: $*" >&2
  exit 1
}

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

# The pins are read line by line, as scripts/module-toolchain.sh reads them: they are data, and
# a stray command in them must not run. The pin decides the target; a core module for a target
# without the component model is wrapped into a component below.
pins_file=modules/toolchain.pins
wasm_target=""
if [ -f "$pins_file" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    case "$line" in '' | '#'*) continue ;; esac
    case "$line" in
      WASM_TARGET=*) wasm_target="${line#WASM_TARGET=}" ;;
    esac
  done <"$pins_file"
fi
[ -n "$wasm_target" ] || fail "WASM_TARGET is missing from $pins_file"

# Cargo's target directory for the module workspace: the machine-local Cargo config redirects
# it, CI uses modules/target, so ask Cargo rather than guessing.
target_dir="$(cargo metadata --manifest-path modules/Cargo.toml --format-version 1 --no-deps --locked |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$target_dir" ] || fail "cannot read the target directory of modules/Cargo.toml"

# The value of frozen field $2 of the [package.metadata.p1-module] table in manifest $1, as
# written, or the empty string when the table does not carry it.
manifest_field() {
  awk -v key="$2" '
    /^[[:space:]]*\[/ { inside = ($0 ~ /^\[package\.metadata\.p1-module\][[:space:]]*$/); next }
    !inside { next }
    /^[[:space:]]*(#|$)/ { next }
    {
      eq = index($0, "=")
      if (eq == 0) next
      name = substr($0, 1, eq - 1)
      gsub(/[[:space:]]/, "", name)
      if (name != key) next
      value = substr($0, eq + 1)
      sub(/^[[:space:]]+/, "", value)
      sub(/[[:space:]]+$/, "", value)
      print value
      exit
    }
  ' "$1"
}

# The names of the `capabilities = ["a", "b"]` list in $1, one per line, or nothing when the
# value is not such a list.
list_items() {
  local value="$1" text="$1" item
  case "$value" in
    \[*\]) ;;
    *) return 0 ;;
  esac
  text="${value#[}"
  text="${text%]}"
  while IFS= read -r item; do
    item="${item#"${item%%[![:space:]]*}"}"
    item="${item%"${item##*[![:space:]]}"}"
    item="${item%\"}"
    item="${item#\"}"
    [ -n "$item" ] && printf '%s\n' "$item"
  done < <(tr ',' '\n' <<<"$text")
}

# The per-class capability allocation of docs/design/modules/wit.md, "Per-class capability
# allocation" (freeze item 13), hard-coded until S0.7 moves it into frozen data. `types` is not
# in it: it carries types only and grants nothing.
allocation() {
  case "$1" in
    tool) echo "completion clock control notices process random snapshot workers workflows workspace workspace-mutation" ;;
    provider) echo "clock control credential-control http notices random websocket" ;;
    context-policy) echo "clock completion control notices summary" ;;
    authorization-policy) echo "clock control notices" ;;
    workflow-implementation) echo "clock control notices random workers workflows" ;;
    *) echo "" ;;
  esac
}

# JSON text for $1 as a string literal; the fields it carries are package-authored.
json_string() {
  local text="$1"
  text="${text//\\/\\\\}"
  text="${text//\"/\\\"}"
  printf '"%s"' "$text"
}

# Whether the wasm file $1 is a component or a core module, read from its header: the layer is
# in the version field. Anything else is refused rather than guessed at.
wasm_layer() {
  local header
  header="$(od -An -tx1 -N8 "$1" | tr -d ' \n')"
  case "$header" in
    0061736d01000000) echo core ;;
    0061736d0d000100) echo component ;;
    *) echo unknown ;;
  esac
}

# Every interface $1's extracted world imports, sorted and unique: the `import <interface>;`
# lines and the `use <interface>.{...}` a world adds for the types it shares. Only the
# interface, never the types it is used for; the version stops before the `.` a `use` opens.
imported_interfaces() {
  awk '
    /^[[:space:]]*(import|use)[[:space:]]/ {
      if (match($0, /[A-Za-z0-9][A-Za-z0-9_.-]*:[A-Za-z0-9][A-Za-z0-9_.-]*\/[A-Za-z0-9][A-Za-z0-9_.-]*(@[0-9]+(\.[0-9]+)*([+-][A-Za-z0-9.-]+)?)?/))
        print substr($0, RSTART, RLENGTH)
    }
  ' "$1" | LC_ALL=C sort -u
}

# The text of a quoted manifest value; a value written another way comes back as written.
unquote() {
  local value="$1"
  case "$value" in
    \"*\")
      value="${value#\"}"
      value="${value%\"}"
      ;;
  esac
  printf '%s' "$value"
}

# Builds one package: checks its manifest, builds it for the pinned target, wraps a core module
# into a component, validates the component and publishes the five build outputs.
build_package() {
  local pkg="$1"
  local manifest="modules/$pkg/Cargo.toml"
  local out="modules/target/p1-modules/$pkg"
  local wasm="$out/$pkg.wasm"
  local core="$target_dir/$wasm_target/release/${pkg//-/_}.wasm"
  local field value kind name namespace world=""

  local -A frozen=()
  for field in name kind world protocol capabilities variant; do
    value="$(manifest_field "$manifest" "$field")"
    if [ -z "$value" ]; then
      fail "$pkg: manifest is missing the frozen field $field ($manifest)"
    fi
    frozen[$field]="$value"
  done
  for field in name kind world protocol variant; do
    frozen[$field]="$(unquote "${frozen[$field]}")"
  done

  kind="${frozen[kind]}"
  case "$kind" in
    tool | provider | context-policy | authorization-policy | workflow-implementation) ;;
    *) fail "$pkg: kind is ${kind}, one of tool, provider, context-policy, authorization-policy, workflow-implementation is required" ;;
  esac

  world="p1:module/$kind@1.0.0"
  [ "${frozen[world]}" = "$world" ] ||
    fail "$pkg: world is ${frozen[world]}, expected the world of its kind, $world"

  name="${frozen[name]}"
  case "$name" in
    */*) ;;
    *) fail "$pkg: name is $name, a name has the form <namespace>/<name>" ;;
  esac
  namespace="${name%%/*}"
  [ "$namespace" = p1 ] ||
    fail "$pkg: name is $name, and this repository builds only official packages in the reserved p1/ namespace"

  [[ "${frozen[protocol]}" =~ ^[0-9]+\.[0-9]+$ ]] ||
    fail "$pkg: protocol is ${frozen[protocol]}, a major.minor version (the version of the protocol the module speaks) is required"

  local variant="${frozen[variant]}"
  [ -n "$variant" ] || fail "$pkg: variant is empty, a module variant is required"

  local -a caps=()
  mapfile -t caps < <(list_items "${frozen[capabilities]}")
  [ "${#caps[@]}" -gt 0 ] ||
    fail "$pkg: capabilities is ${frozen[capabilities]}, a list of capability names is required"
  local allowed cap
  allowed=" $(allocation "$kind") "
  for cap in "${caps[@]}"; do
    [[ "$allowed" == *" $cap "* ]] ||
      fail "$pkg: capability $cap is not in the $kind allocation (docs/design/modules/wit.md)"
  done

  cargo build --manifest-path modules/Cargo.toml --locked --release --target "$wasm_target" -p "$pkg"
  [ -f "$core" ] || fail "$pkg: cargo built no module at $core"

  rm -rf "$out"
  mkdir -p "$out"
  case "$(wasm_layer "$core")" in
    component) cp "$core" "$wasm" ;;
    core)
      # A target without the component model builds a core module; the component model layer is
      # added here, so the target pin alone decides.
      wasm-tools component new "$core" -o "$wasm"
      ;;
    *) fail "$pkg: $core is neither a wasm module nor a wasm component" ;;
  esac

  wasm-tools validate "$wasm"
  wasm-tools component wit "$wasm" >"$out/$pkg.wit"
  (cd "$out" && sha256sum "$pkg.wasm") >"$out/$pkg.sha256"
  imported_interfaces "$out/$pkg.wit" >"$out/$pkg.imports"
  local digest size
  digest="$(awk '{print $1}' "$out/$pkg.sha256")"
  size="$(wc -c <"$wasm" | tr -d ' ')"

  local caps_json=""
  for cap in "${caps[@]}"; do
    [ -z "$caps_json" ] || caps_json+=", "
    caps_json+="$(json_string "$cap")"
  done
  local protocol="${frozen[protocol]}"
  cat >"$out/$pkg.manifest.json" <<EOF
{
  "name": $(json_string "$name"),
  "kind": $(json_string "$kind"),
  "world": $(json_string "${frozen[world]}"),
  "protocol": $(json_string "$protocol"),
  "capabilities": [$caps_json],
  "variant": $(json_string "$variant"),
  "digest": "sha256:$digest",
  "size": $size
}
EOF

  echo "build-modules: $pkg ok sha256:$digest ($size bytes)"
}

built=0
for p in "${packages[@]}"; do
  build_package "$p"
  built=$((built + 1))
done

# `--all` reports the batch; one named package reports only itself.
[ -n "$package" ] || echo "build-modules: $built package(s) built"
