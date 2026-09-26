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
#
# `--shipping` (D059, approved for S0 in ANSWERS B-S7-2) is a separate mode, not a second
# check of the packages: it prints the production dependency graph of the shipping binary `p1`
# (`cargo tree --locked --offline -p p1-host -e normal`), the classification of every
# workspace crate in it from the frozen ADR-0081 table this script carries as data, the
# shipped packages' imports, and every `extension` crate the graph still reaches — a native
# fallback, listed rather than hidden. It builds nothing, reads no component and needs no
# module toolchain. Its last line is
#   check-module-boundaries: shipping: clean (<n> crates, <k> packages, 0 native fallbacks)
# or `check-module-boundaries: shipping: <f> native fallback(s)`; before the cutover it is red
# by design and the gate does not run it.
set -uo pipefail
cd "$(dirname "$0")/.."
root="$(pwd -P)"

output_dir="modules/target/p1-modules"
shipping=0
release_manifest=""
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
--shipping          the shipping audit (D059): print the production dependency graph of the
                    binary `p1`, restricted to workspace crates, the classification of each
                    crate against the frozen ADR-0081 table, the shipped packages' imports and
                    every extension crate still reachable as a native fallback. Reads no
                    component and needs no wasm-tools; combines with --output-dir.
--release-manifest <file>
                    with --shipping, take the shipped packages from the components of this
                    release manifest instead of from the build outputs under --output-dir.
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
    --shipping)
      shipping=1
      shift
      ;;
    --release-manifest)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      release_manifest="$2"
      shift 2
      ;;
    --release-manifest=*)
      release_manifest="${1#*=}"
      shift
      ;;
    *)
      echo "check-module-boundaries: unknown argument $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

# The release manifest names the shipped packages of the shipping audit; the default mode reads
# no manifest, so the option alone is a usage error rather than a silently ignored argument.
if [ -n "$release_manifest" ] && [ "$shipping" -eq 0 ]; then
  echo "check-module-boundaries: --release-manifest is a shipping option; add --shipping" >&2
  usage >&2
  exit 2
fi

# The package checks read the built components with wasm-tools and compare them to the frozen
# allocation; the shipping audit reads the shipping binary's graph and the build outputs'
# manifests, so it needs neither.
if [ "$shipping" -eq 0 ]; then
  command -v wasm-tools >/dev/null 2>&1 ||
    tool_error "wasm-tools is not on PATH (the pinned module toolchain)"
  [ -f modules/capabilities.toml ] ||
    tool_error "modules/capabilities.toml is missing (the frozen capability allocation)"
fi

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

# ---------------------------------------------------------------------------------------
# --shipping (D059): the shipping audit of the binary `p1`
#
# The graph is the production dependency graph of the composition root,
# `cargo tree --locked --offline -p p1-host -e normal`, restricted to workspace crates: cargo
# prints a path dependency with its path, so a line whose path lies below this checkout is one
# of ours and every other line is an external crate — counted, never listed. Every workspace
# crate is then classified from the frozen table below, which is the ADR-0081 line as data
# (D059 keeps it here rather than in modules/capabilities.toml, which holds the per-class
# capability allocation). An `extension` crate the graph still reaches is a native fallback:
# the audit prints it with the path from `p1-host` and the module package that implements the
# same thing, so a compiled-in tool, provider or policy can never hide behind the module
# loader; an extension implementation inside a *foundation* crate is named as a "native twin".
# The shipped packages' imports come from the build outputs under --output-dir, or from the
# components of the release manifest --release-manifest names.

# The frozen classification table: `<crate>|<class>|<reason>`, one crate per line. The class is
# `foundation` (a component ADR-0081 keeps native), `runtime` (the module runtime and the
# protocol it speaks), `core` or `contracts`, or `extension` (a tool, provider, context-policy
# or authorization-policy implementation that becomes a module). A crate of the shipping graph
# that is not listed is a finding: the mode never guesses a class.
SHIPPING_TABLE='
p1-assembly|foundation|assembles environments, profiles and routes and reads the module lock; assembly is a host step (ADR-0081)
p1-auth|foundation|the credential source is native and no interface returns a value (ADR-0081)
p1-contracts|contracts|the contracts the core and its modules share (ADR-0002)
p1-context|extension|holds the summarizing context policy (src/engine.rs); it becomes the context-policy module and its native driver leaves with it (ADR-0036, ADR-0081)
p1-core|core|the core runs one loop and depends only on contracts (ADR-0002)
p1-hook-shadow|foundation|the brain shadow hook is spawned detached by the host and fails open (ADR-0058)
p1-host|foundation|the composition root: the OS services it owns (the terminal driver, the worker service, the detached hook shadow) stay native (ADR-0081); its native authorization policies are named as a native twin
p1-journal|foundation|the session record is native and the single truth, including the version and assembly identity (ADR-0021, ADR-0080)
p1-model-profile|foundation|model policy is host data read at assembly, not an extension (ADR-0004, ADR-0081)
p1-module-protocol|runtime|the value protocol a module speaks; it is a runtime crate (ADR-0081)
p1-module-runtime|runtime|wasmtime, the loader, the executor and the per-contract adapters live in the host, never in the core (ADR-0081)
p1-provider-anthropic|extension|the Anthropic Messages implementation, a provider that becomes a module (ADR-0081)
p1-provider-http|foundation|sending, retry, backoff, the one credential refresh after a 401 or 403 and the read bounds stay native (ADR-0081)
p1-provider-openai|extension|the OpenAI Responses implementation, a provider that becomes a module (ADR-0081)
p1-provider-openai-chat|extension|the Chat Completions implementation, a provider that becomes a module (ADR-0081)
p1-redact|foundation|credential-shape masking runs over the output of every assembled tool (issue #142)
p1-tool-delegate|extension|the worker_start, worker_result, worker_continue and worker_cancel tool members (ADR-0081)
p1-tool-edit|extension|the `edit` tool implementation, which becomes a tool module (ADR-0081)
p1-tool-finish|extension|the `finish` tool and the output contract it checks, which become a tool module (ADR-0081)
p1-tool-patch|extension|the `apply_patch` tool implementation, which becomes a tool module (ADR-0081)
p1-tool-read|extension|the `read` tool implementation, which becomes a tool module (ADR-0081)
p1-tool-search|extension|the `grep` tool implementation, which becomes a tool module (ADR-0081)
p1-tool-shell|extension|the `shell` tool logic is extension; the crate also holds the bubblewrap boundary and the native process service (S3.1), so it stays extension until that service leaves it (ADR-0081)
p1-tool-workflow|extension|the workflow_start, workflow_status, workflow_result and workflow_cancel tool members (ADR-0081)
p1-tool-write|extension|the `write` tool implementation, which becomes a tool module (ADR-0081)
p1-tui|foundation|the terminal driver is native; the state machine it renders is not a module (ADR-0043)
p1-usage|foundation|usage and cost accounting is native (ADR-0019)
p1-workers|foundation|the in-process worker service is native; the worker packages answer the tool members over the module interfaces (ADR-0027, ADR-0081)
p1-workflow|foundation|the workflow interpreter keeps all run state and drives a run (ADR-0081)
p1-workspace|foundation|confinement resolves paths after symlinks and every write is the native atomic replacement (ADR-0081)
'

# The module packages that implement what an extension crate still implements natively, as
# `<crate>|<build output directory>`, one crate per line; a crate with no line ships no package
# yet, which its fallback line says.
SHIPPING_PACKAGES='
p1-context|p1-module-context
p1-tool-delegate|p1-module-worker-start p1-module-worker-continue p1-module-worker-result p1-module-worker-cancel
p1-tool-workflow|p1-module-workflow-start p1-module-workflow-status p1-module-workflow-result p1-module-workflow-cancel
'

# The extension implementations a *foundation* crate still answers natively, as
# `<crate>|<what>|<package directory> [...]`, one line each. The audit names them (a `native
# twin:` line) rather than leaving them inside a class that would hide them; the class of the
# crate they live in does not decide whether the audit may hide them.
SHIPPING_TWINS='
p1-host|the native authorization policies p1/policy/ask and p1/policy/full-access (crates/p1-host/src/policy.rs)|p1-module-policy-ask p1-module-policy-full-access
'

# The class the frozen table gives crate $1, or nothing when the table does not list the crate.
shipping_class() {
  awk -F'|' -v crate="$1" '$1 == crate { print $2; exit }' <<<"$SHIPPING_TABLE"
}

# The frozen ADR-0081 reason the table gives crate $1.
shipping_reason() {
  awk -F'|' -v crate="$1" '$1 == crate { print $3; exit }' <<<"$SHIPPING_TABLE"
}

# The build output directory of every module package that implements crate $1, one per line.
shipping_implementations() {
  awk -F'|' -v crate="$1" '$1 == crate { print $2; exit }' <<<"$SHIPPING_PACKAGES" |
    tr ' ' '\n' | sed '/^$/d'
}

# The parser of `cargo tree --prefix depth` output, used in two passes: with
# `-v count=externals` it prints the number of external crates, otherwise one
# `<depth>\t<name>\t<parent>\t<path>` line per distinct workspace crate in tree order, where the
# root of the tree (depth 0) carries `-` as its parent. A workspace crate is a crate cargo
# prints with a path below $root; cargo prints the path of a repeated crate too (its line
# carries ` (*)`), and only the first line names it, so the tree is the line order together
# with the depth. An external crate is counted once per `name vversion` — cargo prints a
# repeated one again with ` (*)`, and one crate can be reached at two versions — so the
# external figure has the same base as the deduplicated workspace figure next to it.
SHIPPING_TREE='
  {
    if (match($0, /^[0-9]+/)) {
      depth = substr($0, 1, RLENGTH) + 0
      rest = substr($0, RLENGTH + 1)
    } else next
    if (match(rest, /^[A-Za-z0-9_.-]+ v[^ ]+/)) {
      id = substr(rest, 1, RLENGTH)
      name = id
      sub(/ v.*$/, "", name)
    } else next
    path = ""
    if (match(rest, /\((\/[^)]*)\)/)) path = substr(rest, RSTART + 1, RLENGTH - 2)
    if (substr(path, 1, length(root) + 1) != root "/") {
      if (!(id in external_seen)) {
        external_seen[id] = 1
        externals += 1
      }
      next
    }
    max = (depth > max) ? depth : max
    for (d = depth + 1; d <= max; d++) delete stack[d]
    max = depth
    parent = (depth == 0) ? "-" : stack[depth - 1]
    stack[depth] = name
    if (count) next
    if (name in seen) next
    seen[name] = 1
    print depth "\t" name "\t" parent "\t" path
  }
  END { if (count) print externals + 0 }
'

# The component entries of the release manifest $1 as `<name>\t<kind>`, one per line, read with
# python3 because the manifest is JSON (scripts/stage-release.sh reads package manifests the
# same way). A manifest that cannot be read, or carries no components list, is refused: the
# audit prints no inventory it cannot stand behind.
shipping_release_components() {
  python3 - "$1" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as handle:
        manifest = json.load(handle)
except (OSError, ValueError) as exc:
    raise SystemExit(f"check-module-boundaries: cannot read the release manifest {path}: {exc}")

components = manifest.get("components") if isinstance(manifest, dict) else None
if not isinstance(components, list):
    raise SystemExit(f"check-module-boundaries: {path} is no release manifest with components")

for entry in components:
    name = entry.get("name") if isinstance(entry, dict) else None
    kind = entry.get("kind") if isinstance(entry, dict) else None
    if not isinstance(name, str) or not name or not isinstance(kind, str) or not kind:
        raise SystemExit(f"check-module-boundaries: {path}: a component entry names no package and kind")
    print(f"{name}\t{kind}")
PY
}

# The chain of workspace crates from `p1-host` down to crate $1, ` > `-joined: the path through
# the shipping graph by which the graph reaches the crate (shipping_audit's arrays).
shipping_chain() {
  local chain="" name="$1" i="${crate_index[$1]:-}"
  if [ -z "$i" ]; then
    printf '%s' "$name"
    return 0
  fi
  while :; do
    chain="${crate_name[$i]}${chain:+ > $chain}"
    name="${crate_parent[$i]}"
    [ -n "$name" ] || break
    i="${crate_index[$name]:-}"
    if [ -z "$i" ]; then
      chain="$name > $chain"
      break
    fi
  done
  printf '%s' "$chain"
}

# The module packages $* as `<manifest name> (<directory>)`, comma separated, with
# `<directory> (not built)` for a package the build outputs do not hold
# (shipping_audit's arrays).
shipping_package_names() {
  local dir name entry out=""
  for dir in "$@"; do
    [ -n "$dir" ] || continue
    name=""
    for entry in "${!out_dir[@]}"; do
      if [ "${out_dir[$entry]}" = "$dir" ]; then
        name="${out_name[$entry]}"
        break
      fi
    done
    if [ -z "$name" ]; then
      out="${out:+$out, }$dir (not built)"
    else
      out="${out:+$out, }$name ($dir)"
    fi
  done
  printf '%s' "$out"
}

# The module packages that implement crate $1, as shipping_package_names formats them.
shipping_implemented_by() {
  local -a dirs=()
  mapfile -t dirs < <(shipping_implementations "$1")
  [ "${#dirs[@]}" -gt 0 ] || return 0
  shipping_package_names "${dirs[@]}"
}

# The shipping audit (D059): the production graph of `p1-host`, the classification of every
# workspace crate in it, the shipped packages' imports and every extension implementation the
# graph still reaches. Exits 0 only when the graph reaches no extension implementation.
shipping_audit() {
  local tree="" graph="" externals=0 crate_count=0 package_count=0 fallbacks=0
  local depth=0 name="" parent="" path="" class="" reason="" dir="" cname="" ckind="" entry=0
  local imports="" twins="" implements=""
  local -a crate_name=() crate_parent=() crate_class=()
  local -A crate_index=()
  local -a out_dir=() out_name=() out_kind=() out_shipped=()
  local i=0 index=0 manifest=""

  if ! tree="$(cargo tree --locked --offline -p p1-host -e normal --prefix depth)"; then
    tool_error "cannot read the production graph of p1-host (cargo tree --locked --offline -p p1-host -e normal)"
  fi
  externals="$(printf '%s\n' "$tree" | awk -v root="$root" -v count=externals "$SHIPPING_TREE")"
  graph="$(printf '%s\n' "$tree" | awk -v root="$root" "$SHIPPING_TREE")"
  crate_count="$(printf '%s\n' "$graph" | grep -c . || true)"

  echo "check-module-boundaries: shipping: graph: $crate_count workspace crates, $externals external crates (cargo tree --locked --offline -p p1-host -e normal)"

  # The graph restricted to workspace crates, in tree order with the depth that places each
  # crate, and its class from the frozen table.
  while IFS=$'\t' read -r depth name parent path; do
    [ -n "$name" ] || continue
    class="$(shipping_class "$name")"
    if [ -z "$class" ]; then
      echo "check-module-boundaries: shipping: FINDING: $name (depth $depth) is not in the frozen classification table"
      findings=$((findings + 1))
      continue
    fi
    # `read` collapses an empty field between tabs, so the root's parent is written as `-`.
    [ "$parent" = - ] && parent=""
    crate_name+=("$name")
    crate_parent+=("$parent")
    crate_class+=("$class")
    crate_index[$name]="$((${#crate_name[@]} - 1))"
    echo "check-module-boundaries: shipping: graph: $depth $name ($class: $(shipping_reason "$name"))"
  done <<<"$graph"

  # The build outputs: every package directory under --output-dir that carries the package's own
  # manifest and its imports. A directory without either is a finding, never a skipped package.
  for dir in "$output_dir"/*/; do
    [ -d "$dir" ] || continue
    dir="$(basename "$dir")"
    if [ ! -f "$output_dir/$dir/$dir.manifest.json" ]; then
      echo "check-module-boundaries: shipping: FINDING: $output_dir/$dir/$dir.manifest.json is missing"
      findings=$((findings + 1))
      continue
    fi
    if [ ! -f "$output_dir/$dir/$dir.imports" ]; then
      echo "check-module-boundaries: shipping: FINDING: $output_dir/$dir/$dir.imports is missing"
      findings=$((findings + 1))
      continue
    fi
    name="$(json_string_field "$output_dir/$dir/$dir.manifest.json" name)"
    ckind="$(json_string_field "$output_dir/$dir/$dir.manifest.json" kind)"
    if [ -z "$name" ] || [ -z "$ckind" ]; then
      echo "check-module-boundaries: shipping: FINDING: $output_dir/$dir/$dir.manifest.json names no package and kind"
      findings=$((findings + 1))
      continue
    fi
    out_dir+=("$dir")
    out_name+=("$name")
    out_kind+=("$ckind")
    out_shipped+=(1)
  done

  # With a release manifest, the manifest decides what ships: every component entry must have
  # its build output, whose kind it must agree with, and a build output the manifest does not
  # name is not shipped.
  if [ -n "$release_manifest" ]; then
    if ! manifest="$(shipping_release_components "$release_manifest")"; then
      exit 2
    fi
    echo "check-module-boundaries: shipping: imports: the components of $release_manifest"
    index=0
    while [ "$index" -lt "${#out_shipped[@]}" ]; do
      out_shipped[index]=0
      index=$((index + 1))
    done
    while IFS=$'\t' read -r cname ckind; do
      [ -n "$cname" ] || continue
      index=-1
      entry=0
      while [ "$entry" -lt "${#out_name[@]}" ]; do
        if [ "${out_name[$entry]}" = "$cname" ]; then
          index="$entry"
          break
        fi
        entry=$((entry + 1))
      done
      if [ "$index" -lt 0 ]; then
        echo "check-module-boundaries: shipping: FINDING: $release_manifest names $cname, and no build output under $output_dir does"
        findings=$((findings + 1))
        continue
      fi
      if [ "$ckind" != "${out_kind[$index]}" ]; then
        echo "check-module-boundaries: shipping: FINDING: $cname is kind $ckind in $release_manifest and ${out_kind[$index]} in its build output"
        findings=$((findings + 1))
      fi
      out_shipped[index]=1
    done <<<"$manifest"
    index=0
    while [ "$index" -lt "${#out_dir[@]}" ]; do
      if [ "${out_shipped[$index]}" -eq 0 ]; then
        echo "check-module-boundaries: shipping: not shipped: ${out_name[$index]} (${out_dir[$index]}) is built and $release_manifest does not name it"
      fi
      index=$((index + 1))
    done
  fi

  # Every shipped package's imports, its class first so the inventory is grouped by class.
  while IFS=$'\t' read -r ckind dir cname; do
    [ -n "$dir" ] || continue
    imports="$(paste -sd, "$output_dir/$dir/$dir.imports" | sed 's/,/, /g')"
    [ -n "$imports" ] || imports="none"
    echo "check-module-boundaries: shipping: imports: $ckind $dir ($cname): $imports"
    package_count=$((package_count + 1))
  done < <(
    index=0
    while [ "$index" -lt "${#out_dir[@]}" ]; do
      if [ "${out_shipped[$index]}" -eq 1 ]; then
        printf '%s\t%s\t%s\n' "${out_kind[$index]}" "${out_dir[$index]}" "${out_name[$index]}"
      fi
      index=$((index + 1))
    done | LC_ALL=C sort
  )
  if [ "$package_count" -eq 0 ]; then
    echo "check-module-boundaries: shipping: imports: no build output under $output_dir"
  fi

  # Every extension crate the graph reaches is a native fallback, printed with the path from
  # `p1-host` and the module package that implements the same thing. A reachable extension crate
  # that is not printed is the bug this mode exists to prevent.
  for i in "${!crate_name[@]}"; do
    [ "${crate_class[$i]}" = extension ] || continue
    fallbacks=$((fallbacks + 1))
    implements="$(shipping_implemented_by "${crate_name[$i]}")"
    if [ -n "$implements" ]; then
      echo "check-module-boundaries: shipping: native fallback: ${crate_name[$i]} (extension) via $(shipping_chain "${crate_name[$i]}"); implements $implements"
    else
      echo "check-module-boundaries: shipping: native fallback: ${crate_name[$i]} (extension) via $(shipping_chain "${crate_name[$i]}"); no module package ships it yet"
    fi
  done

  # The extension implementations a foundation crate answers natively: named rather than left
  # inside a class that would hide them, and not a crate of the graph, so they are not one of
  # the crate fallbacks counted above.
  while IFS='|' read -r name reason twins; do
    [ -n "$name" ] || continue
    echo "check-module-boundaries: shipping: native twin: $name (extension: $reason) via $(shipping_chain "$name"); implements $(shipping_package_names $twins)"
  done <<<"$SHIPPING_TWINS"

  if [ "$fallbacks" -gt 0 ]; then
    echo "check-module-boundaries: shipping: $fallbacks native fallback(s)"
    exit 1
  fi
  if [ "$findings" -gt 0 ]; then
    echo "check-module-boundaries: shipping: $findings finding(s)"
    exit 1
  fi
  echo "check-module-boundaries: shipping: clean ($crate_count crates, $package_count packages, 0 native fallbacks)"
  exit 0
}

if [ "$shipping" -eq 1 ]; then
  shipping_audit
fi

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
