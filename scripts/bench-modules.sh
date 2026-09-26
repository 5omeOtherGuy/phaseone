#!/usr/bin/env bash
# Measures what p1's WebAssembly modules cost, in one of two suites.
#
# --suite baseline (S0.8) records what the module runtime costs on this box: the cold and warm
# readings of `crates/p1-module-tests/src/bin/bench-baseline.rs`, the box's max RSS for them,
# the sizes of the binaries involved, and the storage of the two build trees — each bound to
# the commit, the compiler and the module toolchain pins it was recorded with.
# Facts, not a check: the baseline record carries no target and no threshold, and the gate does
# not run this suite. One box and one commit say what the runtime costs there; the programme
# lead compares the records.
#
# --suite acceptance (S7.6) measures the complete p1 path — serialization, the host adapter,
# the component call, the capability operation, redaction, queueing and the journal boundary —
# against PLAN §10's performance table, and reports every row with its threshold. The table,
# verbatim from PLAN §10 (as quoted in .wasm/down/ANSWERS.md `## B-S7-1`):
#
#   | Measurement | Initial acceptance target |
#   |---|---|
#   | Warm no-op boundary, 1 KiB payload | p95 added latency ≤1 ms |
#   | Warm read-tool adapter overhead, excluding disk | p95 ≤2 ms |
#   | Provider event forwarding | p95 added latency ≤2 ms; p99 ≤10 ms at 200 events/s |
#   | 1 MiB request/history transfer and validation | p95 ≤25 ms |
#   | 32 MiB synthetic history transfer and validation | p95 ≤300 ms |
#   | Warm installed CLI startup | Added p95 ≤250 ms |
#   | First assembly with uncached component compilation | ≤3 seconds for normal shipped environment |
#   | Cancellation of guest CPU or provider wait | p99 ≤100 ms after cancellation becomes runnable |
#   | Process cancellation | Existing termination/escalation/reaping contract; measured separately |
#   | Idle tool/policy instance | Aim ≤8 MiB committed memory |
#   | Idle provider instance | Aim ≤16 MiB committed memory |
#   | Representative 16-agent workload | Added steady-state RSS ≤400 MiB over native baseline |
#   | Repeated identical workload | No continuing resource/RSS growth after warm-up |
#
# plus PLAN §11 risk 5's resolving experiment, "1 MiB/32 MiB histories and 16-agent
# repeated-compaction benchmark", as the row compaction-16.
#
# The measurements are the #[ignore]d cases of crates/p1-module-tests/tests/acceptance.rs, run
# in the release profile one case per test process, over the components scripts/build-modules.sh
# builds (this script builds them first). Each case prints one JSON line. cancel-process runs
# the process service's existing termination/escalation/reaping tests (p1-tool-shell), in the
# gate's debug profile, and reports their result. A row whose component does not exist yet is
# PENDING and names the missing component and the stream that owns it; PENDING is never a pass.
# The gate never runs this suite; it runs scripts/test_bench_modules.py.
#
# Exit codes:
#   baseline:   0 when the record is written, 1 when a fact could not be measured,
#               2 on a usage error.
#   acceptance with --check:
#               0 when every selected row was measured and passes;
#               1 when a measured row fails its threshold;
#               3 when no row failed, but at least one row is PENDING because its component
#                 does not exist yet;
#               2 on a usage or tooling error (a build, a case or the contract run that did not
#                 produce its measurement; such a row is reported as ERROR).
#   acceptance without --check: the rows are reported with the verdict MEASURED or PENDING;
#               0 when every measurable row was measured, 2 on a usage or tooling error.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
usage: scripts/bench-modules.sh --suite baseline [--out <path>]
       scripts/bench-modules.sh --suite acceptance [--row <id>]... [--check] [--json <file>]
       scripts/bench-modules.sh --help

--suite <name>  the suite to run: baseline or acceptance
--out <path>    baseline: where the record goes
                (default .worker-scratch/bench-baseline-<shortsha>.txt)
--row <id>      acceptance: run only this row (repeatable; default every row)
--check         acceptance: compare each measured row against its threshold
--json <file>   acceptance: also write the rows as JSON to <file>

The baseline suite builds missing module packages, builds the bench binary in the debug
profile the gate uses, runs it under /usr/bin/time -v, and writes one record of wall time,
max RSS, binary size and build storage, with the commit, rustc -V and the module toolchain
pins. The record is printed and its path is the last line.
Exit 0 when the record is written, 1 when a fact could not be measured, 2 on a usage error.

The acceptance suite prints one line per row, `<row id>  <measured value>  <threshold>
<verdict>  <reason>`, and a summary line after them. With --check: exit 0 when every
selected row passes, 1 when a row fails, 3 when none fails but one is PENDING, 2 on a usage
or tooling error. Without --check: exit 0, or 2 on a usage or tooling error.
EOF
}

# PLAN §10's thresholds, one row per line: id | PLAN's target, verbatim | the checks the
# script evaluates | who measures the row. These numbers are PLAN's and the only thresholds
# this suite knows. A threshold changes only through a PR that carries measured workload
# evidence and is reviewed by the reviewer role — never to turn a red run green; PLAN §10
# says a failing limit is repaired in copying, instance lifetime or runtime configuration
# first, by the stream that owns the path.
#
# A check is `<statistic><=<limit><unit>` (the case must report that statistic in that
# unit, and passes when its value is at most the limit) or `growth=none` (the case reports
# whether a resource kept growing). The measured-by column is `case <test name>`,
# `contract` (cancel-process) or `pending <missing component>/<owning stream>`.
# compaction-16 also needs the history rows within their targets, which it will check
# beside its own growth once the context-policy component exists.
ACCEPTANCE_TABLE='
boundary-noop-1k|p95 added latency ≤1 ms|added-p95<=1ms|case boundary_noop_1k
read-adapter|p95 ≤2 ms|p95<=2ms|pending read-tool component/S2
provider-events|p95 added latency ≤2 ms; p99 ≤10 ms at 200 events/s|added-p95<=2ms p99<=10ms|pending provider component/S4
history-1m|p95 ≤25 ms|p95<=25ms|case history_1m
history-32m|p95 ≤300 ms|p95<=300ms|case history_32m
cli-startup|Added p95 ≤250 ms|added-p95<=250ms|case cli_startup
first-assembly|≤3 seconds for normal shipped environment|max<=3s|pending host module assembly (wasm-loader-v1)/S1
cancel-guest|p99 ≤100 ms after cancellation becomes runnable|p99<=100ms|case cancel_guest
cancel-process|Existing termination/escalation/reaping contract; measured separately|contract|contract
idle-tool|Aim ≤8 MiB committed memory|per-instance<=8MiB|case idle_tool
idle-provider|Aim ≤16 MiB committed memory|per-instance<=16MiB|pending provider component/S4
agents-16-rss|Added steady-state RSS ≤400 MiB over native baseline|added-steady<=400MiB|case agents_16_rss
steady-growth|No continuing resource/RSS growth after warm-up|growth=none|case steady_growth
compaction-16|No continuing growth; history rows within their targets|growth=none|pending context-policy component/S5
'

suite=""
out=""
rows=()
check=0
json_out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --suite)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      suite="$2"
      shift 2
      ;;
    --out)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      out="$2"
      shift 2
      ;;
    --row)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      rows+=("$2")
      shift 2
      ;;
    --check)
      check=1
      shift
      ;;
    --json)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      json_out="$2"
      shift 2
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

if [ -z "$suite" ]; then
  usage >&2
  exit 2
fi
case "$suite" in
  baseline)
    if [ "${#rows[@]}" -gt 0 ] || [ "$check" = 1 ] || [ -n "$json_out" ]; then
      echo "bench-modules: --row, --check and --json belong to the acceptance suite" >&2
      exit 2
    fi
    ;;
  acceptance)
    if [ -n "$out" ]; then
      echo "bench-modules: --out belongs to the baseline suite; the acceptance suite writes --json" >&2
      exit 2
    fi
    ;;
  *)
    echo "bench-modules: suite $suite is neither baseline nor acceptance" >&2
    exit 2
    ;;
esac

# A tooling error of the acceptance suite: something other than a threshold stopped a row.
tooling() {
  echo "bench-modules: $*" >&2
  exit 2
}

# The machine-local target the gate builds through (scripts/gate.sh does the same); a reading
# taken against another target directory would describe a build the gate never makes.
local_cargo_config() {
  if [ -z "${CI:-}" ] && [ ! -f .cargo/config.toml ]; then
    scripts/local-cargo-config.sh >&2
  fi
}

run_acceptance() {
  local ids=() sources=() id target checks source known selected=() wanted
  while IFS='|' read -r id target checks source; do
    [ -n "$id" ] || continue
    ids+=("$id")
    sources+=("$source")
  done <<<"$ACCEPTANCE_TABLE"

  if [ "${#rows[@]}" -eq 0 ]; then
    selected=("${ids[@]}")
  else
    for wanted in "${rows[@]}"; do
      known=0
      for id in "${ids[@]}"; do [ "$id" = "$wanted" ] && known=1; done
      [ "$known" = 1 ] || tooling "unknown row $wanted; the rows are: ${ids[*]}"
    done
    # Table order, each row once, whatever order and repetition --row came in.
    for id in "${ids[@]}"; do
      for wanted in "${rows[@]}"; do
        if [ "$id" = "$wanted" ]; then
          selected+=("$id")
          break
        fi
      done
    done
  fi

  local cases=() contract=0 index
  for id in "${selected[@]}"; do
    for index in "${!ids[@]}"; do
      [ "${ids[$index]}" = "$id" ] || continue
      case "${sources[$index]}" in
        case\ *) cases+=("${sources[$index]#case }") ;;
        contract) contract=1 ;;
      esac
    done
  done

  local scratch
  scratch="$(mktemp -d)"
  # shellcheck disable=SC2064 # the directory is fixed now, when the trap is set
  trap "rm -rf '$scratch'" EXIT
  printf '%s\n' "$ACCEPTANCE_TABLE" >"$scratch/table"
  printf '%s\n' "${selected[@]}" >"$scratch/selected"
  : >"$scratch/cases.out"
  : >"$scratch/cases.failed"

  if [ "${#cases[@]}" -gt 0 ]; then
    # Stale components would be measured as if they were this commit's.
    echo "bench-modules: building the module packages (scripts/build-modules.sh --all)" >&2
    scripts/build-modules.sh --all >&2 || tooling "scripts/build-modules.sh --all failed"
    local_cargo_config
    local case_name
    for case_name in "${cases[@]}"; do
      if [ "$case_name" = cli_startup ] && [ -z "${P1_BIN:-}" ]; then
        # The native baseline and the module path start the same binary, built in the
        # profile the cases run in.
        echo "bench-modules: building p1 in the release profile for cli-startup" >&2
        cargo build --locked --release -p p1-host --bin p1 >&2 ||
          tooling "cargo build --locked --release -p p1-host --bin p1 failed"
      fi
    done
    echo "bench-modules: building the acceptance cases in the release profile" >&2
    cargo test --locked --release -p p1-module-tests --test acceptance --no-run >&2 ||
      tooling "the acceptance cases do not build"
    # One test process per case, so no memory reading carries what an earlier case left.
    for case_name in "${cases[@]}"; do
      echo "bench-modules: measuring $case_name" >&2
      if ! cargo test --locked --release -p p1-module-tests --test acceptance -- \
        --ignored --nocapture --test-threads=1 --exact "$case_name" >>"$scratch/cases.out"; then
        echo "$case_name" >>"$scratch/cases.failed"
      fi
    done
  fi

  local contract_status=skipped
  if [ "$contract" = 1 ]; then
    echo "bench-modules: running the process cancellation contract (p1-tool-shell)" >&2
    local_cargo_config
    if cargo test --locked -p p1-tool-shell --test lead_group_cleanup --test process_service \
      --test review >"$scratch/contract.out" 2>&1; then
      contract_status=0
    else
      contract_status=1
      tail -n 40 "$scratch/contract.out" >&2
    fi
  fi
  touch "$scratch/contract.out"

  local commit
  commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
  local status=0
  PYTHONUTF8=1 python3 - "$scratch" "$check" "$contract_status" "$commit" "$json_out" \
    <<'PY' || status=$?
import json
import re
import sys

def main():
    scratch, check, contract_status, commit, json_out = sys.argv[1:6]
    check = check == "1"

    table = {}
    order = []
    for line in open(f"{scratch}/table", encoding="utf-8"):
        line = line.rstrip("\n")
        if not line:
            continue
        row_id, target, checks, source = line.split("|")
        table[row_id] = {"target": target, "checks": checks.split(), "source": source}
        order.append(row_id)
    selected = [line.strip() for line in open(f"{scratch}/selected", encoding="utf-8") if line.strip()]
    failed_cases = {line.strip() for line in open(f"{scratch}/cases.failed", encoding="utf-8") if line.strip()}

    # One JSON object per case; libtest prints `test <name> ... ` before it on the same line.
    reported = {}
    for line in open(f"{scratch}/cases.out", encoding="utf-8"):
        at = line.find('{"acceptance-row"')
        if at < 0:
            continue
        try:
            value = json.loads(line[at:])
        except json.JSONDecodeError:
            continue
        reported[value["acceptance-row"]] = value

    form = re.compile(r"^(?P<statistic>[a-z0-9-]+)<=(?P<limit>[0-9]+(?:\.[0-9]+)?)(?P<unit>[A-Za-z]+)$")


    def evaluate(spec, measurements):
        """(passed, text, problem) of one check; problem is set when it cannot be evaluated."""
        if spec == "growth=none":
            found = [m for m in measurements if m.get("statistic") == "growth"]
            if not found:
                return None, "", "the case reported no growth"
            value = found[0].get("value")
            return value == "none", f"growth {value}", f"growth {value}, not none"
        match = form.match(spec)
        if not match:
            return None, "", f"the table's check {spec} is not a known form"
        statistic, limit, unit = match["statistic"], float(match["limit"]), match["unit"]
        found = [m for m in measurements if m.get("statistic") == statistic]
        if not found:
            return None, "", f"the case reported no {statistic}"
        measured = found[0]
        if measured.get("unit") != unit:
            return None, "", f"{statistic} came in {measured.get('unit')}, the threshold is in {unit}"
        value = measured.get("value")
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            return None, "", f"{statistic} is not a number"
        text = f"{statistic} {value:g} {unit}"
        if value <= limit:
            return True, text, None
        return False, text, f"{statistic} {value:g} {unit} > {limit:g} {unit}"


    results = []
    for row_id in [row for row in order if row in selected]:
        row = table[row_id]
        source = row["source"]
        result = {"id": row_id, "threshold": row["target"], "checks": row["checks"],
                  "measurements": [], "samples": None, "detail": "", "value": "-"}
        if source.startswith("pending "):
            component, stream = source[len("pending "):].rsplit("/", 1)
            result.update(verdict="PENDING", reason=f"no {component} exists yet (owner {stream})",
                          missing=component, owner=stream)
        elif source == "contract":
            text = open(f"{scratch}/contract.out", encoding="utf-8").read()
            passed = sum(int(n) for n in re.findall(r"test result: \w+\. (\d+) passed", text))
            failed = sum(int(n) for n in re.findall(r"test result: \w+\. \d+ passed; (\d+) failed", text))
            result["value"] = f"contract {passed} passed, {failed} failed"
            result["detail"] = "p1-tool-shell lead_group_cleanup, process_service and review tests"
            if contract_status == "0":
                result.update(verdict="PASS" if check else "MEASURED",
                              reason="termination/escalation/reaping contract tests pass (measured separately)")
            elif "test result: FAILED" in text:
                result.update(verdict="FAIL" if check else "MEASURED",
                              reason="termination/escalation/reaping contract tests fail")
            else:
                result.update(verdict="ERROR", reason="the contract tests did not run to a result")
        else:
            case = source[len("case "):]
            value = reported.get(row_id)
            if value is None:
                why = "failed" if case in failed_cases else "printed no measurement"
                result.update(verdict="ERROR", reason=f"case {case} {why}")
            else:
                measurements = value.get("measurements", [])
                result.update(measurements=measurements, samples=value.get("samples"),
                              detail=value.get("detail", ""))
                texts, problems, failures = [], [], []
                for spec in row["checks"]:
                    passed, text, problem = evaluate(spec, measurements)
                    if passed is None:
                        problems.append(problem)
                        continue
                    texts.append(text)
                    if not passed:
                        failures.append(problem)
                result["value"] = "; ".join(texts) if texts else "-"
                about = f"{result['samples']} samples; {result['detail']}"
                if problems:
                    result.update(verdict="ERROR", reason="; ".join(problems))
                elif not check:
                    result.update(verdict="MEASURED", reason=about)
                elif failures:
                    result.update(verdict="FAIL", reason="; ".join(failures) + f" ({about})")
                else:
                    result.update(verdict="PASS", reason=about)
        results.append(result)

    for result in results:
        print(f"{result['id']:<16}  {result['value']}  {result['threshold']}  "
              f"{result['verdict']}  {result['reason']}")

    counts = {verdict: sum(1 for r in results if r["verdict"] == verdict)
              for verdict in ("PASS", "FAIL", "PENDING", "MEASURED", "ERROR")}
    if counts["ERROR"]:
        code = 2
    elif not check:
        code = 0
    elif counts["FAIL"]:
        code = 1
    elif counts["PENDING"]:
        code = 3
    else:
        code = 0
    if check:
        summary = (f"bench-modules: acceptance --check: {len(results)} rows: {counts['PASS']} pass, "
                   f"{counts['FAIL']} fail, {counts['PENDING']} pending, {counts['ERROR']} error; exit {code}")
    else:
        summary = (f"bench-modules: acceptance: {len(results)} rows: {counts['MEASURED']} measured, "
                   f"{counts['PENDING']} pending, {counts['ERROR']} error; exit {code}")

    if json_out:
        document = {"suite": "acceptance", "commit": commit, "check": check, "rows": results,
                    "summary": {key.lower(): value for key, value in counts.items()}, "exit": code}
        try:
            with open(json_out, "w", encoding="utf-8") as handle:
                json.dump(document, handle, indent=2, ensure_ascii=False)
                handle.write("\n")
        except OSError as error:
            print(f"bench-modules: cannot write {json_out}: {error}", file=sys.stderr)
            code = 2
            summary = summary.rsplit(";", 1)[0] + f"; exit {code}"
    print(summary)
    return code


try:
    sys.exit(main())
except Exception as error:  # a broken report is a tooling error, never exit 1
    print(f"bench-modules: the report failed: {error!r}", file=sys.stderr)
    sys.exit(2)
PY
  exit "$status"
}

if [ "$suite" = acceptance ]; then
  run_acceptance
fi

fail() {
  echo "bench-modules: $*" >&2
  exit 1
}

[ -x /usr/bin/time ] || fail "/usr/bin/time is missing, so the bench's max RSS cannot be measured"

package=p1-module-fixture
package_dir="modules/target/p1-modules/$package"
fixture_wasm="$package_dir/$package.wasm"

# The fixture is the module every reading is about; the build publishes it, so a missing one
# is built rather than recorded as an absent number.
if [ ! -f "$fixture_wasm" ] || [ ! -f "$package_dir/$package.manifest.json" ]; then
  echo "bench-modules: the fixture package is not built; running scripts/build-modules.sh --all" >&2
  scripts/build-modules.sh --all >&2
fi

# The gate builds through this checkout's machine-local target (scripts/gate.sh does the
# same); a record taken against another target directory would describe a build the gate
# never makes.
if [ -z "${CI:-}" ] && [ ! -f .cargo/config.toml ]; then
  scripts/local-cargo-config.sh >&2
fi

echo "bench-modules: building the bench binary in the debug profile" >&2
cargo build --locked -p p1-module-tests --bin bench-baseline >&2
# The record names the runtime's own rlib, which Cargo lifts into the target directory's
# debug/ only when the library is itself a target of the build.
cargo build --locked -p p1-module-runtime >&2

target_dir="$(cargo metadata --format-version 1 --no-deps --locked |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$target_dir" ] || fail "cannot read the target directory of this workspace"
bench_bin="$target_dir/debug/bench-baseline"
rlib="$target_dir/debug/libp1_module_runtime.rlib"
[ -x "$bench_bin" ] || fail "the bench binary is not at $bench_bin"

# The readings and /usr/bin/time's report are two answers from one process: the bench's own
# output on stdout, the report on stderr.
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
bench_out="$scratch/bench.txt"
time_report="$scratch/time.txt"
if ! /usr/bin/time -v "$bench_bin" >"$bench_out" 2>"$time_report"; then
  cat "$time_report" >&2
  fail "the bench binary failed at $bench_bin"
fi

max_rss_kb="$(awk -F': ' '/Maximum resident set size \(kbytes\)/ {print $2}' "$time_report")"
[ -n "$max_rss_kb" ] || fail "cannot read the max RSS from /usr/bin/time -v"

file_bytes() {
  local path="$1"
  [ -f "$path" ] || fail "missing $path"
  wc -c <"$path" | tr -d ' '
}

dir_bytes() {
  local path="$1"
  [ -d "$path" ] || fail "missing $path"
  du -sb "$path" | awk '{print $1}'
}

head_sha="$(git rev-parse HEAD)"
short_sha="$(git rev-parse --short HEAD)"
rustc_line="$(rustc -V)"
toolchain="$(scripts/module-toolchain.sh --check)" ||
  fail "scripts/module-toolchain.sh --check failed, so the record's toolchain is unverified"

record="${out:-.worker-scratch/bench-baseline-$short_sha.txt}"
mkdir -p "$(dirname "$record")" || fail "cannot create $(dirname "$record")"
{
  printf 'bench-modules: baseline record\n\n'
  printf 'suite: %s\n' "$suite"
  printf 'date -u: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'git HEAD: %s\n' "$head_sha"
  printf 'rustc -V: %s\n' "$rustc_line"
  printf '\n-- bench binary output (%s)\n' "$bench_bin"
  cat "$bench_out"
  printf '\n-- max RSS (/usr/bin/time -v of the bench binary)\n'
  printf 'max_rss: %s kB (%s MiB)\n' "$max_rss_kb" \
    "$(awk -v kb="$max_rss_kb" 'BEGIN { printf "%.1f", kb / 1024 }')"
  printf '\n-- binary sizes\n'
  printf 'fixture_wasm: %s bytes (%s)\n' "$(file_bytes "$fixture_wasm")" "$fixture_wasm"
  printf 'bench_binary: %s bytes (%s)\n' "$(file_bytes "$bench_bin")" "$bench_bin"
  printf 'libp1_module_runtime_rlib: %s bytes (%s)\n' "$(file_bytes "$rlib")" "$rlib"
  printf '\n-- build storage (du -sb)\n'
  printf 'modules_target: %s bytes (%s)\n' "$(dir_bytes modules/target)" "modules/target"
  printf 'cargo_target: %s bytes (%s)\n' "$(dir_bytes "$target_dir")" "$target_dir"
  printf '\n-- module toolchain (scripts/module-toolchain.sh --check)\n'
  printf '%s\n' "$toolchain"
} >"$record"

cat "$record"
printf 'bench-modules: baseline recorded (%s)\n' "$record"
