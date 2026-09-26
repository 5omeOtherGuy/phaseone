#!/usr/bin/env bash
# Measures what p1's WebAssembly modules cost, in one of three suites.
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
# --suite streaming (S4.8) measures the three provider components while a stream runs: the
# added latency of forwarding one event, whether the stream stays incremental (the scripted
# transport hands out one SSE block per poll and reports every poll that asked for a block
# before the consumer acknowledged the event before it), what a slow consumer sees, and the
# process's own peak RSS over a stream of `P1_STREAM_EVENTS` (default 100000) text deltas and
# over ten times that many. PLAN §10's `provider-events` row is the only PLAN threshold this
# suite applies; every other row is this suite's own bound, which the row names, and the memory
# rows carry their measured values as facts (ANSWERS S4-B6, D076). The cases are the cases of
# crates/p1-module-tests/tests/provider_streaming.rs, run in the release profile one case per
# test process; the memory row runs its case twice, once at N and once at 10·N events. The
# summary line names that workload, so a run shortened with P1_STREAM_EVENTS cannot be mistaken
# for the design workload; P1_STREAM_EVENTS must be a positive integer, and any other value is a
# usage error (exit 2).
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
#   streaming:  0 when every row passed, 1 when a row failed its threshold or this suite's own
#               bound, 2 on a usage or tooling error (a case that did not produce its
#               measurement is reported as ERROR).
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
usage: scripts/bench-modules.sh --suite baseline [--out <path>]
       scripts/bench-modules.sh --suite acceptance [--row <id>]... [--check] [--json <file>]
       scripts/bench-modules.sh --suite streaming
       scripts/bench-modules.sh --help

--suite <name>  the suite to run: baseline, acceptance or streaming
--out <path>    baseline: where the record goes
                (default .worker-scratch/bench-baseline-<shortsha>.txt)
--row <id>      acceptance: run only this row (repeatable; default every row)
--check         acceptance: compare each measured row against its threshold
--json <file>   acceptance: also write the rows as JSON to <file> (relative to the repository root)

The baseline suite builds missing module packages, builds the bench binary in the debug
profile the gate uses, runs it under /usr/bin/time -v, and writes one record of wall time,
max RSS, binary size and build storage, with the commit, rustc -V and the module toolchain
pins. The record is printed and its path is the last line.
Exit 0 when the record is written, 1 when a fact could not be measured, 2 on a usage error.

The acceptance suite prints one line per row, `<row id>  <measured value>  <threshold>
<verdict>  <reason>`, and a summary line after them. With --check: exit 0 when every
selected row passes, 1 when a row fails, 3 when none fails but one is PENDING, 2 on a usage
or tooling error. Without --check: exit 0, or 2 on a usage or tooling error.

The streaming suite prints the same one line per row and a summary line. It takes no other
option: it exits 0 when every row passed, 1 when a row failed, 2 on a usage or tooling error.
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
# notice: S5.9 (issue #333) switches only the compaction-16 row's measured-by column to
# its own case (compaction_16, PLAN §11 risk 5 over the context-policy component S5.2
# built); every other row, threshold and check here is S7's (#297) and unchanged.
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
compaction-16|No continuing growth; history rows within their targets|growth=none|case compaction_16
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
  streaming)
    if [ -n "$out" ] || [ "${#rows[@]}" -gt 0 ] || [ "$check" = 1 ] || [ -n "$json_out" ]; then
      echo "bench-modules: --out, --row, --check and --json belong to the baseline and acceptance suites" >&2
      exit 2
    fi
    ;;
  *)
    echo "bench-modules: suite $suite is neither baseline, acceptance nor streaming" >&2
    exit 2
    ;;
esac

# A tooling error of the acceptance suite: something other than a threshold stopped a row.
tooling() {
  echo "bench-modules: $*" >&2
  exit 2
}

# The machine-local target the gate builds through (scripts/gate.sh does the same); a reading
# taken against another target directory would describe a build the gate never makes. A helper
# that refuses the configuration (no HOME, a target root that is not ext4) is a tooling error of
# this suite, so it leaves through `tooling` rather than with the helper's own status.
local_cargo_config() {
  if [ -z "${CI:-}" ] && [ ! -f .cargo/config.toml ]; then
    scripts/local-cargo-config.sh >&2 || tooling "scripts/local-cargo-config.sh failed"
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
  scratch="$(mktemp -d)" || tooling "cannot create a scratch directory"
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
    # Every contract test binary runs even when one fails, so the row counts them all.
    if cargo test --locked --no-fail-fast -p p1-tool-shell --test lead_group_cleanup \
      --test process_service --test review >"$scratch/contract.out" 2>&1; then
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
  # The report classifies every row itself and exits 0..3; any other status means python3 did
  # not run it (a missing interpreter is 127), which is a tooling error, never a failed row.
  case "$status" in
    0 | 1 | 2 | 3) ;;
    *) tooling "the report exited $status without classifying a row" ;;
  esac
  exit "$status"
}

# The streaming rows (S4.8), one row per line: id | the row's threshold, verbatim from PLAN
# §10 where one exists | the checks the script evaluates | the kind of row | who measures it.
#
# A check is `<statistic><=<limit><unit>`: the case must report that statistic in that unit and
# the row passes when its value is at most the limit. The kinds are:
#   threshold  PLAN §10's row is this row's threshold; failing it fails the suite.
#   bound      this suite's own bound (the row names it); failing it fails the suite. The row's
#              value carries the measurements as facts, since PLAN §10 has no row for them.
# The measured-by column is `case <test name>` (one test process) or `memory <test name>` (two
# test processes, one at N and one at 10·N events, so the row's growth is one reading).
#
# The latency check is PLAN §10's `provider-events` row, in the units the case reports: the
# case measures the added latency of one forwarded event (the transport releases a block, the
# consumer receives its event) and reports p95/p99 in microseconds. PLAN §10 paces that row at
# 200 events/s; the case drives events back to back, never waiting on the provider, so its
# reading is the forwarding cost itself and the rate it observed is printed beside it.
STREAMING_TABLE='
latency-anthropic|PLAN §10: Provider event forwarding — p95 added latency ≤2 ms; p99 ≤10 ms at 200 events/s|p95<=2000us p99<=10000us read-ahead<=0chunks|threshold|case streaming_latency_anthropic
latency-responses|PLAN §10: Provider event forwarding — p95 added latency ≤2 ms; p99 ≤10 ms at 200 events/s|p95<=2000us p99<=10000us read-ahead<=0chunks|threshold|case streaming_latency_responses
latency-chat|PLAN §10: Provider event forwarding — p95 added latency ≤2 ms; p99 ≤10 ms at 200 events/s|p95<=2000us p99<=10000us read-ahead<=0chunks|threshold|case streaming_latency_chat
slow-consumer-anthropic|- (S4.8 bound: read-ahead ≤0 chunks; PLAN §10 has no slow-consumer row)|read-ahead<=0chunks|bound|case streaming_slow_consumer_anthropic
slow-consumer-responses|- (S4.8 bound: read-ahead ≤0 chunks; PLAN §10 has no slow-consumer row)|read-ahead<=0chunks|bound|case streaming_slow_consumer_responses
slow-consumer-chat|- (S4.8 bound: read-ahead ≤0 chunks; PLAN §10 has no slow-consumer row)|read-ahead<=0chunks|bound|case streaming_slow_consumer_chat
tool-call-anthropic|- (S4.8 bound: read-ahead ≤0 chunks; PLAN §10 has no tool-call row)|read-ahead<=0chunks|bound|case streaming_tool_call_anthropic
tool-call-responses|- (S4.8 bound: read-ahead ≤0 chunks; PLAN §10 has no tool-call row)|read-ahead<=0chunks|bound|case streaming_tool_call_responses
tool-call-chat|- (S4.8 bound: read-ahead ≤0 chunks; PLAN §10 has no tool-call row)|read-ahead<=0chunks|bound|case streaming_tool_call_chat
memory-anthropic|- (S4.8 bound: peak-RSS growth ≤32 bytes per extra event, N vs 10·N deltas; PLAN §10 has no streaming row, its provider memory row is idle-provider)|growth-per-delta<=32B/delta|bound|memory streaming_peak_rss_anthropic
memory-responses|- (S4.8 bound: peak-RSS growth ≤32 bytes per extra event, N vs 10·N deltas; PLAN §10 has no streaming row, its provider memory row is idle-provider)|growth-per-delta<=32B/delta|bound|memory streaming_peak_rss_responses
memory-chat|- (S4.8 bound: peak-RSS growth ≤32 bytes per extra event, N vs 10·N deltas; PLAN §10 has no streaming row, its provider memory row is idle-provider)|growth-per-delta<=32B/delta|bound|memory streaming_peak_rss_chat
'

# S4.8's streaming suite: the components' own streaming cost. Each row is one case process, and
# a memory row is two (N and 10·N events); the rows and their bounds are STREAMING_TABLE's.
run_streaming() {
  local scratch
  scratch="$(mktemp -d)" || tooling "cannot create a scratch directory"
  # shellcheck disable=SC2064 # the directory is fixed now, when the trap is set
  trap "rm -rf '$scratch'" EXIT
  printf '%s\n' "$STREAMING_TABLE" >"$scratch/table"
  : >"$scratch/cases.out"
  : >"$scratch/cases.failed"

  # The measured workload: the design's 100 000 deltas, and ten times as many for the memory
  # rows' growth. An explicit P1_STREAM_EVENTS is honoured, which keeps a manual run cheap.
  local events="${P1_STREAM_EVENTS:-100000}"
  # The value reaches shell arithmetic and the memory rows' divisor. A non-integer would abort
  # the script on `events * 10`, with the status of a failed row; a word such as `abc` evaluates
  # as 0, which only surfaces as the report's division by zero. So it is validated here, before
  # either, and refused as the usage error it is.
  [[ "$events" =~ ^[1-9][0-9]*$ ]] ||
    tooling "P1_STREAM_EVENTS must be a positive integer, got $events"
  local large="$((events * 10))"

  # Stale components would be measured as if they were this commit's.
  echo "bench-modules: building the module packages (scripts/build-modules.sh --all)" >&2
  scripts/build-modules.sh --all >&2 || tooling "scripts/build-modules.sh --all failed"
  local_cargo_config
  echo "bench-modules: building the streaming cases in the release profile" >&2
  cargo test --locked --release -p p1-module-tests --test provider_streaming --no-run >&2 ||
    tooling "the streaming cases do not build"

  local row_id target checks kind source case_name size
  while IFS='|' read -r row_id target checks kind source; do
    [ -n "$row_id" ] || continue
    case "$source" in
      case\ *)
        case_name="${source#case }"
        echo "bench-modules: measuring $case_name at $events events" >&2
        if ! P1_STREAM_EVENTS="$events" cargo test --locked --release -p p1-module-tests \
          --test provider_streaming -- --nocapture --test-threads=1 --exact "$case_name" \
          >>"$scratch/cases.out"; then
          echo "$case_name" >>"$scratch/cases.failed"
        fi
        ;;
      memory\ *)
        case_name="${source#memory }"
        # One process per size: the peak RSS of one reading must not carry the other's.
        for size in "$events" "$large"; do
          echo "bench-modules: measuring $case_name at $size events" >&2
          if ! P1_STREAM_EVENTS="$size" cargo test --locked --release -p p1-module-tests \
            --test provider_streaming -- --nocapture --test-threads=1 --exact "$case_name" \
            >>"$scratch/cases.out"; then
            echo "$case_name" >>"$scratch/cases.failed"
          fi
        done
        ;;
      *)
        tooling "unknown source $source in the streaming table"
        ;;
    esac
  done <"$scratch/table"

  local commit
  commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
  local status=0
  PYTHONUTF8=1 python3 - "$scratch" "$events" "$large" "$commit" <<'PY' || status=$?
import json
import re
import sys


def objects(line):
    """Every JSON object on `line`, wherever it starts and whatever order its keys are in."""
    for start, char in enumerate(line):
        if char != "{":
            continue
        try:
            value, _ = json.JSONDecoder().raw_decode(line[start:])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            yield value


def main():
    scratch, small, large, commit = sys.argv[1:5]
    small, large = int(small), int(large)

    # id | the row's threshold | the checks | the kind | who measures it
    table = []
    for line in open(f"{scratch}/table", encoding="utf-8"):
        line = line.rstrip("\n")
        if not line:
            continue
        row_id, target, checks, kind, source = line.split("|")
        table.append((row_id, target, checks.split(), kind, source))
    failed_cases = {line.strip() for line in open(f"{scratch}/cases.failed", encoding="utf-8") if line.strip()}

    # One JSON object per case line; libtest prints `test <name> ... ` before it on the same
    # line. The object is found by decoding each `{` on the line and keeping the one that
    # carries the row key, so the report does not depend on the key order serde_json writes.
    # A memory row's case runs twice, so a row can carry more than one reading.
    reported = {}
    for line in open(f"{scratch}/cases.out", encoding="utf-8"):
        for value in objects(line):
            if "streaming-row" in value:
                reported.setdefault(value["streaming-row"], []).append(value)

    form = re.compile(r"^(?P<statistic>[a-z0-9-]+)<=(?P<limit>[0-9]+(?:\.[0-9]+)?)(?P<unit>[A-Za-z/]+)$")

    def found(measurements, statistic):
        for measurement in measurements:
            if measurement.get("statistic") == statistic:
                return measurement
        return None

    def evaluate(spec, measurements):
        """(passed, text, problem) of one check; problem is set when it cannot be evaluated."""
        match = form.match(spec)
        if not match:
            return None, "", f"the table's check {spec} is not a known form"
        statistic, limit, unit = match["statistic"], float(match["limit"]), match["unit"]
        measured = found(measurements, statistic)
        if measured is None:
            return None, "", f"the case reported no {statistic}"
        if measured.get("unit") != unit:
            return None, "", f"{statistic} came in {measured.get('unit')}, the check is in {unit}"
        value = measured.get("value")
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            return None, "", f"{statistic} is not a number"
        text = f"{statistic} {value:g} {unit}"
        if value <= limit:
            return True, text, None
        return False, text, f"{statistic} {value:g} {unit} > {limit:g} {unit}"

    def shown(measurements):
        return "; ".join(
            f"{measurement['statistic']} {measurement['value']:g} {measurement['unit']}"
            for measurement in measurements
        )

    results = []
    for row_id, target, checks, kind, source in table:
        case_name = source.split(" ", 1)[1]
        result = {"id": row_id, "threshold": target, "kind": kind, "value": "-",
                  "samples": 0, "detail": "", "verdict": "", "reason": ""}
        values = reported.get(row_id, [])
        if not values:
            why = "failed" if case_name in failed_cases else "printed no measurement"
            result.update(verdict="ERROR", reason=f"case {case_name} {why}")
            results.append(result)
            continue
        if source.startswith("memory "):
            # Two readings of the same case, one per size; the growth between them is the bound.
            by_size = {}
            for value in values:
                measured_size = found(value.get("measurements", []), "text-deltas")
                delta = found(value.get("measurements", []), "peak-rss-delta")
                if measured_size is None or delta is None:
                    continue
                by_size[int(measured_size["value"])] = delta
            if small not in by_size or large not in by_size:
                result.update(
                    verdict="ERROR",
                    reason=f"the case reported the sizes {sorted(by_size)}, the suite asked for "
                           f"{small} and {large}",
                )
                results.append(result)
                continue
            delta_small, delta_large = by_size[small], by_size[large]
            growth = delta_large["value"] - delta_small["value"]
            # The bound is per extra event, not per workload: the streaming path must cost the
            # same whatever the stream's length is, and a per-event leak of a whole event
            # object is an order of magnitude above the bound.
            per_delta = round(growth * 1024 * 1024 / (large - small), 3)
            measurements = [
                {"statistic": "growth", "value": growth, "unit": "MiB"},
                {"statistic": "growth-per-delta", "value": per_delta, "unit": "B/delta"},
            ]
            result["value"] = (
                f"peak-rss-delta {delta_small['value']:g} MiB at {small} deltas, "
                f"{delta_large['value']:g} MiB at {large} deltas, growth {growth:g} MiB "
                f"({per_delta:g} B per extra delta)"
            )
            result["samples"] = values[-1].get("samples", 0)
            result["detail"] = values[-1].get("detail", "")
        else:
            value = values[0]
            measurements = value.get("measurements", [])
            result["value"] = shown(measurements) or "-"
            result["samples"] = value.get("samples", 0)
            result["detail"] = value.get("detail", "")
        texts, problems, failures = [], [], []
        for spec in checks:
            passed, text, problem = evaluate(spec, measurements)
            if passed is None:
                problems.append(problem)
                continue
            texts.append(text)
            if not passed:
                failures.append(problem)
        if problems:
            result.update(verdict="ERROR", reason="; ".join(problems))
        elif failures:
            result.update(verdict="FAIL", reason="; ".join(failures))
        elif case_name in failed_cases:
            result.update(verdict="FAIL", reason=f"case {case_name} failed")
        else:
            result.update(verdict="PASS", reason=f"{result['samples']} samples; {result['detail']}")
        results.append(result)

    for result in results:
        print(f"{result['id']:<22}  {result['value']}  {result['threshold']}  "
              f"{result['verdict']}  {result['reason']}")

    counts = {verdict: sum(1 for r in results if r["verdict"] == verdict)
              for verdict in ("PASS", "FAIL", "ERROR")}
    if counts["ERROR"]:
        code = 2
    elif counts["FAIL"]:
        code = 1
    else:
        code = 0
    print(f"bench-modules: streaming: {len(results)} rows at {small} events: {counts['PASS']} pass, "
          f"{counts['FAIL']} fail, {counts['ERROR']} error; exit {code}")
    return code


try:
    sys.exit(main())
except Exception as error:  # a broken report is a tooling error, never exit 1
    print(f"bench-modules: the report failed: {error!r}", file=sys.stderr)
    sys.exit(2)
PY
  # The report classifies every row itself and exits 0..2; any other status means python3 did
  # not run it (a missing interpreter is 127), which is a tooling error, never a failed row.
  case "$status" in
    0 | 1 | 2) ;;
    *) tooling "the report exited $status without classifying a row" ;;
  esac
  exit "$status"
}

if [ "$suite" = acceptance ]; then
  run_acceptance
fi

if [ "$suite" = streaming ]; then
  run_streaming
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
