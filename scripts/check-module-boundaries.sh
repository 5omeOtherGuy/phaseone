#!/usr/bin/env bash
# The boundary check of the p1 WebAssembly modules (ADR-0071): freeze item 13, the per-class
# capability allocation as frozen data that a manifest declares and this script compares a
# built component's imports against, and freeze item 11, the unsafe policy for generated
# bindings. Both are published in docs/design/modules/capabilities.md.
#
# The script builds nothing: it reads the outputs scripts/build-modules.sh wrote under
# modules/target/p1-modules/ (or the directory --output-dir names) and the frozen data in
# modules/capabilities.toml. Every check prints one line:
#   check-module-boundaries: <package|crate>: ok
#   check-module-boundaries: <package|crate>: FINDING: <why>
# and the last line is `check-module-boundaries: clean (<n> packages, <m> crates)` when no
# finding was seen, else `check-module-boundaries: <k> finding(s)`.
set -uo pipefail
cd "$(dirname "$0")/.."
root="$(pwd -P)"

output_dir="modules/target/p1-modules"
findings=0
packages_checked=0
crates_checked=0

usage() {
  cat <<'EOF'
usage: scripts/check-module-boundaries.sh [--output-dir <dir>]

Checks the boundary of every module package and crate of the module workspace against the
frozen data in modules/capabilities.toml: a manifest's kind and capabilities, the built
component's imports (cross-checked with `wasm-tools component wit`), the component's world,
and the unsafe policy (a crate inherits unsafe_code = "forbid"; handwritten source uses no
`unsafe`; generated code is reported, not failed). It builds nothing.

--output-dir <dir>  read the build outputs from <dir> instead of modules/target/p1-modules
                    (each package's outputs are <dir>/<package>/<package>.{wasm,wit,imports,
                    manifest.json}); a copy of it is how the check is tried against a
                    tampered import list.
--help, -h          print this help and exit 0.

Exit 0 when clean, 1 when there is a finding, 2 on a usage or tool error.
EOF
}

tool_error() {
  echo "check-module-boundaries: $*" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help | -h)
      usage
      exit 0
      ;;
    --output-dir)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      output_dir="$2"
      shift 2
      ;;
    --output-dir=*)
      output_dir="${1#*=}"
      shift
      ;;
    *)
      echo "check-module-boundaries: unknown argument $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

command -v wasm-tools >/dev/null 2>&1 ||
  tool_error "wasm-tools is not on PATH (the pinned module toolchain)"
[ -f modules/capabilities.toml ] ||
  tool_error "modules/capabilities.toml is missing (the frozen capability allocation)"

# The reasons collected for the package or crate currently checked.
REASONS=()

# Records one reason a check failed.
reason() {
  REASONS+=("$1")
}

# Prints the one check line for $1 and clears the collected reasons: FINDING with them all
# when there were any, ok when there were none. Returns the number of reasons seen.
report() {
  local name="$1" joined="" r
  if [ "${#REASONS[@]}" -eq 0 ]; then
    echo "check-module-boundaries: $name: ok"
    return 0
  fi
  for r in "${REASONS[@]}"; do
    if [ -n "$joined" ]; then joined="$joined; $r"; else joined="$r"; fi
  done
  echo "check-module-boundaries: $name: FINDING: $joined"
  findings=$((findings + ${#REASONS[@]}))
  return "${#REASONS[@]}"
}

# The value of frozen field $3 in table [$2] of TOML file $1, as written, or nothing.
toml_field() {
  awk -v table="$2" -v key="$3" '
    /^[[:space:]]*\[/ {
      header = $0
      sub(/^[[:space:]]*\[/, "", header)
      sub(/\][[:space:]]*$/, "", header)
      inside = (header == table)
      next
    }
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

# The text of a quoted TOML/JSON value; a value written another way comes back as written.
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

# The items of a `[a, b]` list value, one per line, or nothing when it is not such a list.
list_items() {
  local value="$1" text item
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

# The interfaces modules/capabilities.toml allocates to module class $1, one per line.
class_allocation() {
  local value
  value="$(awk -v class="$1" '
    /^[[:space:]]*\[/ {
      header = $0
      sub(/^[[:space:]]*\[/, "", header)
      sub(/\][[:space:]]*$/, "", header)
      inside = (header == class)
      next
    }
    !inside { next }
    /^[[:space:]]*(#|$)/ { next }
    /^[[:space:]]*imports[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*/, "", value)
      print value
      exit
    }
  ' modules/capabilities.toml)"
  list_items "$value"
}

# The module classes capabilities.toml declares, one per line, in file order.
classes() {
  awk '
    /^[[:space:]]*\[[a-z][a-z0-9-]*\][[:space:]]*$/ {
      header = $0
      sub(/^[[:space:]]*\[/, "", header)
      sub(/\][[:space:]]*$/, "", header)
      print header
    }
  ' modules/capabilities.toml
}

# The items of top-level list key $1 of capabilities.toml (before the first table), one per line.
top_list() {
  local value
  value="$(awk -v key="$1" '
    /^[[:space:]]*\[/ { exit }
    $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
      value = $0
      sub(/^[^=]*=[[:space:]]*/, "", value)
      print value
      exit
    }
  ' modules/capabilities.toml)"
  list_items "$value"
}

# Every module package name, sorted: a directory modules/p1-module-*/ whose manifest carries
# the frozen [package.metadata.p1-module] table. Bindings crates are not packages.
module_packages() {
  local dir name
  for dir in modules/p1-module-*/; do
    [ -d "$dir" ] || continue
    name="$(basename "$dir")"
    [ -f "$dir/Cargo.toml" ] || continue
    grep -q '^\[package\.metadata\.p1-module\]' "$dir/Cargo.toml" && echo "$name"
  done | LC_ALL=C sort
}

# Every crate of the module workspace (its members list, globs expanded), sorted, deduplicated.
module_workspace_crates() {
  local value member dir
  value="$(toml_field modules/Cargo.toml workspace members)"
  while IFS= read -r member; do
    [ -n "$member" ] || continue
    for dir in modules/$member; do
      [ -d "$dir" ] && [ -f "$dir/Cargo.toml" ] && printf '%s\n' "$dir"
    done
  done < <(list_items "$value")
}

# The crate name field of Cargo.toml $1, or its directory basename.
crate_name() {
  local name
  name="$(unquote "$(toml_field "$1/Cargo.toml" package name)")"
  [ -n "$name" ] || name="$(basename "$1")"
  printf '%s' "$name"
}

# The awk lexer the unsafe scan uses: prints `file:line` for every line whose code, with
# comments and string/char literals removed, contains the keyword `unsafe`. It tracks block
# comments, strings and raw strings across lines; `'a` is read as a lifetime, not a literal.
LEXER='
function hasword(s, w,   n, arr, i) {
  n = split(s, arr, /[^A-Za-z0-9_]+/)
  for (i = 1; i <= n; i++) if (arr[i] == w) return 1
  return 0
}
BEGIN { inblock = 0; instring = 0; inraw = 0; rawhash = 0 }
{
  line = $0; out = ""; i = 1; n = length(line)
  while (i <= n) {
    c = substr(line, i, 1)
    if (inblock) {
      if (c == "*" && substr(line, i+1, 1) == "/") { inblock = 0; i += 2 } else { i += 1 }
      continue
    }
    if (instring) {
      if (c == "\\") { i += 2; continue }
      if (c == dq) { instring = 0 }
      i += 1
      continue
    }
    if (inraw) {
      if (c == dq) {
        j = i + 1; k = 0
        while (k < rawhash && substr(line, j, 1) == "#") { j += 1; k += 1 }
        if (k == rawhash) { inraw = 0; i = j; continue }
      }
      i += 1
      continue
    }
    if (c == "/" && substr(line, i+1, 1) == "/") break
    if (c == "/" && substr(line, i+1, 1) == "*") { inblock = 1; i += 2; continue }
    if (c == dq) { instring = 1; i += 1; continue }
    if (c == "r" || c == "b" || c == "c") {
      j = i
      if (c != "r") j += 1
      if (substr(line, j, 1) == "r") {
        j += 1; k = 0
        while (substr(line, j, 1) == "#") { k += 1; j += 1 }
        if (substr(line, j, 1) == dq) { inraw = 1; rawhash = k; i = j + 1; continue }
      }
    }
    if (c == sq) {
      nc = substr(line, i+1, 1)
      if (nc == "\\") {
        j = i + 3
        while (j <= n && substr(line, j, 1) != sq) j += 1
        i = j + 1
        continue
      }
      if (substr(line, i+2, 1) == sq) { i += 3; continue }
      i += 1
      continue
    }
    out = out c
    i += 1
  }
  if (hasword(out, "unsafe")) print FILENAME ":" FNR
}'

# `file:line` for every use of the `unsafe` keyword in the code of the .rs files under $1.
unsafe_source_hits() {
  local dir="$1" f
  local -a files=()
  while IFS= read -r -d '' f; do files+=("$f"); done \
    < <(find "$dir" -type f -name '*.rs' -print0 | LC_ALL=C sort -z)
  [ "${#files[@]}" -gt 0 ] || return 0
  awk -v dq="\"" -v sq="'" "$LEXER" "${files[@]}"
}

# The macro that generates the code of crate directory $1, or nothing: code generated by
# wit_bindgen::generate! or wasmtime::component::bindgen! is reported, not failed.
generated_macro() {
  local dir="$1"
  if grep -rqF --include='*.rs' 'wit_bindgen::generate!' "$dir" 2>/dev/null; then
    printf '%s' 'wit_bindgen::generate!'
  elif grep -rqF --include='*.rs' 'component::bindgen!' "$dir" 2>/dev/null; then
    printf '%s' 'wasmtime::component::bindgen!'
  fi
}

# The workspace manifest a crate of directory $1 inherits its lints from.
workspace_manifest() {
  case "$1" in
    modules/*) printf '%s' modules/Cargo.toml ;;
    *) printf '%s' Cargo.toml ;;
  esac
}

# Every interface an extracted world $1 imports, sorted and unique: the `import <interface>;`
# lines and the `use <interface>.{...}` a world adds for shared types. Same extraction as
# scripts/build-modules.sh, so the built <package>.imports and a fresh extraction compare.
imported_interfaces() {
  awk '
    /^[[:space:]]*(import|use)[[:space:]]/ {
      if (match($0, /[A-Za-z0-9][A-Za-z0-9_.-]*:[A-Za-z0-9][A-Za-z0-9_.-]*\/[A-Za-z0-9][A-Za-z0-9_.-]*(@[0-9]+(\.[0-9]+)*([+-][A-Za-z0-9.-]+)?)?/))
        print substr($0, RSTART, RLENGTH)
    }
  ' "$1" | LC_ALL=C sort -u
}

# The export names of a WIT world: `export name: …` for a function and `export interface;`
# for an interface. $2 is the world's name, or `*` for the first world block (a component's
# extracted world is unnamed as `root`).
world_exports() {
  awk -v world="$2" '
    world == "*" { if (!started && /^world /) { started = 1; inside = 1; next } }
    world != "*" { if (!inside && $0 ~ ("^world " world " \\{")) { inside = 1; next } }
    inside && /^}/ { inside = 0; next }
    inside && /^[[:space:]]*export[[:space:]]/ {
      line = $0
      sub(/^[[:space:]]*export[[:space:]]+/, "", line)
      match(line, /^[^ \t]+/)
      token = substr(line, 1, RLENGTH)
      sub(/;.*$/, "", token)
      if (index(token, "/") > 0) { print token }
      else { sub(/:.*$/, "", token); print token }
    }
  ' "$1"
}

# An export name with any package path and version removed: `p1:module/decoding@1.0.0` and
# `decoding` are the same export, as is a function's plain name.
normalize_export() {
  local name="$1"
  name="${name##*/}"
  name="${name%%@*}"
  printf '%s' "$name"
}

# The sorted, unique export set of WIT world $1 as modules/wit/worlds.wit defines it.
wit_world_exports() {
  world_exports modules/wit/worlds.wit "$1" | while IFS= read -r name; do
    normalize_export "$name"
  done | LC_ALL=C sort -u
}

# The sorted, unique export set of the component whose extracted world is $1.
component_exports() {
  world_exports "$1" '*' | while IFS= read -r name; do
    normalize_export "$name"
  done | LC_ALL=C sort -u
}

# The string value of key $2 in the JSON build manifest $1, or nothing.
json_string_field() {
  sed -n "s/^[[:space:]]*\"$2\":[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" | head -1
}

# The item list of key $2 in the JSON build manifest $1, one per line.
json_list_field() {
  local value
  value="$(sed -n "s/^[[:space:]]*\"$2\":[[:space:]]*\[\(.*\)\].*/\1/p" "$1" | head -1)"
  list_items "[$value]"
}

# Checks one module package $1: its manifest against the frozen allocation, its built component
# against its manifest and the manifest's world.
check_package() {
  local pkg="$1"
  local manifest="modules/$pkg/Cargo.toml"
  local out="$output_dir/$pkg"
  local wasm="$out/$pkg.wasm" wit="$out/$pkg.wit"
  local imports_file="$out/$pkg.imports" build_manifest="$out/$pkg.manifest.json"
  local kind world cap iface import file

  REASONS=()
  packages_checked=$((packages_checked + 1))

  for file in "$wasm" "$wit" "$imports_file" "$build_manifest"; do
    if [ ! -f "$file" ]; then
      reason "missing $file; run scripts/build-modules.sh --all"
      report "$pkg"
      return 0
    fi
  done

  kind="$(unquote "$(toml_field "$manifest" package.metadata.p1-module kind)")"
  world="$(unquote "$(toml_field "$manifest" package.metadata.p1-module world)")"

  if ! classes | grep -qxF "$kind"; then
    reason "kind $kind is not a class in modules/capabilities.toml"
    report "$pkg"
    return 0
  fi

  # The manifest's world is the world of its kind, and the built manifest agrees.
  [ "$world" = "p1:module/$kind@1.0.0" ] ||
    reason "world $world is not the $kind world p1:module/$kind@1.0.0"
  local built_world built_kind
  built_world="$(json_string_field "$build_manifest" world)"
  [ "$built_world" = "$world" ] ||
    reason "the built manifest's world $built_world is not the manifest's $world"
  built_kind="$(json_string_field "$build_manifest" kind)"
  [ "$built_kind" = "$kind" ] ||
    reason "the built manifest's kind $built_kind is not the manifest's $kind"

  # Every manifest capability is in the class allocation, and the built manifest carries them.
  local allocation declared built_caps
  allocation="$(class_allocation "$kind")"
  declared="$(list_items "$(toml_field "$manifest" package.metadata.p1-module capabilities)")"
  while IFS= read -r cap; do
    [ -n "$cap" ] || continue
    printf '%s\n' "$allocation" | grep -qxF "$cap" ||
      reason "capability $cap is not in the $kind allocation (modules/capabilities.toml)"
  done <<<"$declared"
  built_caps="$(json_list_field "$build_manifest" capabilities)"
  [ "$(printf '%s\n' "$declared" | LC_ALL=C sort -u)" = "$(printf '%s\n' "$built_caps" | LC_ALL=C sort -u)" ] ||
    reason "the built manifest's capabilities are not the manifest's"

  # The component's world is the manifest's world: its exports are the world's exports.
  local expected actual
  expected="$(wit_world_exports "$kind")"
  actual="$(component_exports "$wit")"
  [ "$expected" = "$actual" ] ||
    reason "the component's exports are not the $kind world's exports"

  # The <package>.imports file is the component's own import list, extracted fresh.
  local fresh extracted
  fresh="$(mktemp)"
  if ! wasm-tools component wit "$wasm" >"$fresh" 2>/dev/null; then
    rm -f "$fresh"
    reason "wasm-tools could not extract the world from $wasm"
  else
    extracted="$(mktemp)"
    imported_interfaces "$fresh" >"$extracted"
    if ! LC_ALL=C sort -u "$imports_file" | diff -q - "$extracted" >/dev/null 2>&1; then
      reason "$pkg.imports does not match the component's extracted world"
    fi
    rm -f "$fresh" "$extracted"
  fi

  # Every import is a p1:module interface of the class allocation and either a type-only
  # interface or a capability the manifest declares. Any wasi: import is always a finding.
  local type_only
  type_only="$(top_list type-only)"
  while IFS= read -r import; do
    [ -n "$import" ] || continue
    case "$import" in
      wasi:*)
        reason "imports $import: the boundary refuses every wasi: import (D-XO-4)"
        continue
        ;;
      p1:module/*@1.0.0) ;;
      *)
        reason "imports $import, not a p1:module interface of the manifest's world"
        continue
        ;;
    esac
    iface="${import#p1:module/}"
    iface="${iface%%@*}"
    if ! printf '%s\n' "$allocation" | grep -qxF "$iface"; then
      reason "imports $iface, not in the $kind allocation"
      continue
    fi
    if printf '%s\n' "$type_only" | grep -qxF "$iface"; then
      continue
    fi
    if ! printf '%s\n' "$declared" | grep -qxF "$iface"; then
      reason "imports $iface, which the manifest does not declare"
    fi
  done < <(LC_ALL=C sort -u "$imports_file")

  report "$pkg"
}

# Checks one crate directory $1: it inherits unsafe_code = "forbid", its handwritten source
# uses no `unsafe`, and generated code is named and reported, not failed.
check_crate() {
  local dir="$1"
  local name ws macro hits
  name="$(crate_name "$dir")"
  ws="$(workspace_manifest "$dir")"

  REASONS=()
  crates_checked=$((crates_checked + 1))

  if [ "$(unquote "$(toml_field "$dir/Cargo.toml" lints workspace)")" != "true" ]; then
    reason "does not inherit the workspace lints ([lints] workspace = true)"
  elif [ "$(unquote "$(toml_field "$ws" workspace.lints.rust unsafe_code)")" != "forbid" ]; then
    reason "$ws does not set unsafe_code = \"forbid\""
  fi

  macro="$(generated_macro "$dir")"
  if [ -z "$macro" ]; then
    hits="$(unsafe_source_hits "$dir")"
    if [ -n "$hits" ]; then
      reason "handwritten source uses the unsafe keyword: $(printf '%s' "$hits" | paste -sd' ' -)"
    fi
  fi

  if [ "${#REASONS[@]}" -gt 0 ]; then
    report "$name"
  elif [ -n "$macro" ]; then
    echo "check-module-boundaries: $name: ok generated by $macro: its expansion contains unsafe code the lint does not see because it expands from another crate's macro; the crate compiles under unsafe_code = \"forbid\""
  else
    echo "check-module-boundaries: $name: ok"
  fi
}

mapfile -t packages < <(module_packages)
for pkg in "${packages[@]}"; do
  check_package "$pkg"
done

# The crates whose policy is checked: the module workspace's own crates, every path dependency
# of a package that lives outside modules/ (a shared guest crate under crates/), and the three
# host crates of the runtime, its protocol and its tests. Deduplicated, in a stable order.
mapfile -t crates < <(
  {
    module_workspace_crates
    for pkg in "${packages[@]}"; do
      while IFS= read -r dep; do
        [ -n "$dep" ] || continue
        abs="$(realpath -m "$root/modules/$pkg/$dep" 2>/dev/null)" || continue
        case "$abs" in
          "$root/modules"/*) ;;
          *) [ -f "$abs/Cargo.toml" ] && printf '%s\n' "$abs" ;;
        esac
      done < <(sed -n 's/.*path[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "modules/$pkg/Cargo.toml")
    done
    printf '%s\n' crates/p1-module-runtime crates/p1-module-protocol crates/p1-module-tests
  } | LC_ALL=C sort -u
)
for dir in "${crates[@]}"; do
  [ -d "$dir" ] && [ -f "$dir/Cargo.toml" ] || continue
  check_crate "$dir"
done

if [ "$findings" -eq 0 ]; then
  echo "check-module-boundaries: clean ($packages_checked packages, $crates_checked crates)"
  exit 0
fi
echo "check-module-boundaries: $findings finding(s)"
exit 1
