#!/usr/bin/env python3
"""Tests for scripts/bench-modules.sh --suite acceptance — stdlib only.

The script runs in a temporary repository whose scripts/ holds stub helpers and whose PATH
holds a stub cargo. The stub logs every call and answers an acceptance case with the JSON
line a test put in STUB_CASES/<case>.json (or fails the case when there is none), and the
process contract run with the result STUB_CONTRACT names. Real cargo never runs, so these
tests prove the script's argument handling, classification, exit codes and output, never a
measurement.
"""
from __future__ import annotations

import json
import os
import pathlib
import re
import shutil
import stat
import subprocess
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "bench-modules.sh"

# PLAN §10's targets as the report must print them, in the table's order, with the owner of
# each row that has no component yet.
ROWS = [
    ("boundary-noop-1k", "p95 added latency ≤1 ms", None),
    ("read-adapter", "p95 ≤2 ms", "S2"),
    ("provider-events", "p95 added latency ≤2 ms; p99 ≤10 ms at 200 events/s", "S4"),
    ("history-1m", "p95 ≤25 ms", None),
    ("history-32m", "p95 ≤300 ms", None),
    ("cli-startup", "Added p95 ≤250 ms", None),
    ("first-assembly", "≤3 seconds for normal shipped environment", "S1"),
    ("cancel-guest", "p99 ≤100 ms after cancellation becomes runnable", None),
    ("cancel-process", "Existing termination/escalation/reaping contract; measured separately", None),
    ("idle-tool", "Aim ≤8 MiB committed memory", None),
    ("idle-provider", "Aim ≤16 MiB committed memory", "S4"),
    ("agents-16-rss", "Added steady-state RSS ≤400 MiB over native baseline", None),
    ("steady-growth", "No continuing resource/RSS growth after warm-up", None),
    ("compaction-16", "No continuing growth; history rows within their targets", "S5"),
]
PENDING = [row for row, _, owner in ROWS if owner]

# A passing reading of every measured row: (case, statistic, value, unit).
PASSING = {
    "boundary-noop-1k": ("boundary_noop_1k", "added-p95", 0.1, "ms"),
    "history-1m": ("history_1m", "p95", 12.0, "ms"),
    "history-32m": ("history_32m", "p95", 200.0, "ms"),
    "cli-startup": ("cli_startup", "added-p95", 60.0, "ms"),
    "cancel-guest": ("cancel_guest", "p99", 5.0, "ms"),
    "idle-tool": ("idle_tool", "per-instance", 0.5, "MiB"),
    "agents-16-rss": ("agents_16_rss", "added-steady", 150.0, "MiB"),
    "steady-growth": ("steady_growth", "growth", "none", ""),
}

CARGO_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    printf '%s\\n' "cargo $*" >> "$STUB_LOG"
    case " $* " in
      *" --no-run "*|"  build "*|" build "*)
        [ -z "${STUB_BUILD_FAIL:-}" ] || { echo "stub: build fails" >&2; exit 101; }
        exit 0 ;;
      *" -p p1-tool-shell "*)
        case "${STUB_CONTRACT:-ok}" in
          ok) echo "test result: ok. 2 passed; 0 failed; 0 ignored"
              echo "test result: ok. 1 passed; 0 failed; 0 ignored"
              exit 0 ;;
          failed) echo "test result: FAILED. 1 passed; 1 failed; 0 ignored"; exit 101 ;;
          *) echo "error: could not compile p1-tool-shell"; exit 101 ;;
        esac ;;
      *" --test acceptance "*)
        name="${@: -1}"
        if [ -f "$STUB_CASES/$name.json" ]; then
          printf 'test %s ... %s\\n' "$name" "$(cat "$STUB_CASES/$name.json")"
          echo "test result: ok. 1 passed; 0 failed"
          exit 0
        fi
        echo "test $name ... FAILED"
        echo "stub: case $name panicked" >&2
        exit 101 ;;
    esac
    echo "stub cargo: unexpected call $*" >&2
    exit 99
    """
)

HELPER_STUB = "#!/usr/bin/env bash\nprintf '%s\\n' \"$(basename \"$0\") $*\" >> \"$STUB_LOG\"\nexit 0\n"

# The machine-local target helper refuses some boxes (no HOME, a target root that is not ext4);
# STUB_LOCAL_CARGO_CONFIG_FAIL makes that refusal happen here.
LOCAL_CARGO_CONFIG_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    printf '%s\\n' "local-cargo-config.sh $*" >> "$STUB_LOG"
    if [ -n "${STUB_LOCAL_CARGO_CONFIG_FAIL:-}" ]; then
      echo "stub: local-cargo-config refuses this box" >&2
      exit 1
    fi
    exit 0
    """
)

# mktemp and python3 never fail in these tests unless asked to: the stubs delegate to the real
# tools, and STUB_MKTEMP_FAIL / STUB_PYTHON3_FAIL stand in for a box where they do not run.
MKTEMP_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    if [ -n "${STUB_MKTEMP_FAIL:-}" ]; then
      echo "stub: mktemp fails" >&2
      exit 1
    fi
    exec /usr/bin/mktemp "$@"
    """
)

PYTHON3_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env bash
    if [ -n "${STUB_PYTHON3_FAIL:-}" ]; then
      echo "stub: python3: command not found" >&2
      exit 127
    fi
    exec /usr/bin/python3 "$@"
    """
)


def write_exec(path: pathlib.Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class Harness:
    def __init__(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="bench-modules-test-")
        base = pathlib.Path(self.tmp.name)
        self.base = base
        self.repo = base / "repo"
        self.cases = base / "cases"
        self.cases.mkdir()
        self.log = base / "calls.log"
        scripts = self.repo / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(SCRIPT, scripts / "bench-modules.sh")
        write_exec(scripts / "build-modules.sh", HELPER_STUB)
        write_exec(scripts / "local-cargo-config.sh", LOCAL_CARGO_CONFIG_STUB)
        write_exec(base / "bin" / "cargo", CARGO_STUB)
        write_exec(base / "bin" / "mktemp", MKTEMP_STUB)
        write_exec(base / "bin" / "python3", PYTHON3_STUB)
        self.env = {
            "PATH": f"{base / 'bin'}:/usr/bin:/bin",
            "HOME": str(base),
            "STUB_LOG": str(self.log),
            "STUB_CASES": str(self.cases),
            "LC_ALL": "C",
            "CI": "",
        }

    def reading(self, row: str, value=None, statistic=None, unit=None) -> None:
        case, default_statistic, default_value, default_unit = PASSING[row]
        line = {
            "acceptance-row": row,
            "measurements": [{
                "statistic": statistic or default_statistic,
                "value": default_value if value is None else value,
                "unit": default_unit if unit is None else unit,
            }],
            "samples": 40,
            "detail": f"stub reading of {row}",
        }
        (self.cases / f"{case}.json").write_text(json.dumps(line), encoding="utf-8")

    def all_passing(self) -> None:
        for row in PASSING:
            self.reading(row)

    def run(self, *args: str, **extra: str) -> subprocess.CompletedProcess[str]:
        env = dict(self.env)
        env.update(extra)
        return subprocess.run(
            ["bash", str(self.repo / "scripts" / "bench-modules.sh"), *args],
            cwd=self.base,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=120,
            check=False,
        )

    def calls(self) -> list[str]:
        if not self.log.exists():
            return []
        return self.log.read_text(encoding="utf-8").splitlines()

    def measured_cases(self) -> list[str]:
        return [call.split()[-1] for call in self.calls()
                if " --test acceptance " in call and "--exact" in call]

    def cleanup(self) -> None:
        self.tmp.cleanup()


def row_lines(stdout: str) -> dict[str, list[str]]:
    """The report's row lines, split into their columns, by row id."""
    lines = {}
    for line in stdout.splitlines():
        if line.startswith("bench-modules:"):
            continue
        columns = re.split(r"\s{2,}", line)
        lines[columns[0]] = columns
    return lines


def verdicts(stdout: str) -> dict[str, str]:
    return {row: columns[3] for row, columns in row_lines(stdout).items()}


class BenchModulesTests(unittest.TestCase):
    def harness(self) -> Harness:
        h = Harness()
        self.addCleanup(h.cleanup)
        return h

    # ---- arguments ------------------------------------------------------------------

    def test_usage_errors_exit_2_and_measure_nothing(self) -> None:
        h = self.harness()
        for args in (
            [],
            ["--suite"],
            ["--suite", "nightly"],
            ["--suite", "acceptance", "--out", "record.txt"],
            ["--suite", "baseline", "--check"],
            ["--suite", "baseline", "--row", "history-1m"],
            ["--suite", "baseline", "--json", "rows.json"],
            ["--suite", "acceptance", "--row"],
            ["--suite", "acceptance", "--json"],
            ["--suite", "acceptance", "--row", "no-such-row"],
            ["--suite", "acceptance", "--bogus"],
            ["--check"],
        ):
            with self.subTest(args=args):
                result = h.run(*args)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(h.calls(), [])

    def test_an_unknown_row_is_named_with_the_known_ones(self) -> None:
        h = self.harness()
        result = h.run("--suite", "acceptance", "--row", "no-such-row")
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown row no-such-row", result.stderr)
        self.assertIn("history-32m", result.stderr)

    def test_help_describes_both_suites(self) -> None:
        result = self.harness().run("--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("--suite baseline", result.stdout)
        self.assertIn("--suite acceptance", result.stdout)
        for flag in ("--row", "--check", "--json"):
            self.assertIn(flag, result.stdout)

    def test_the_facts_not_a_check_statement_stays_with_baseline_only(self) -> None:
        header = SCRIPT.read_text(encoding="utf-8").split("set -euo pipefail")[0]
        statement = header.index("Facts, not a check")
        self.assertLess(header.index("--suite baseline"), statement)
        self.assertLess(statement, header.index("--suite acceptance"))
        self.assertEqual(header.count("Facts, not a check"), 1)

    # ---- the whole suite ------------------------------------------------------------

    def test_the_whole_suite_reports_every_row_in_plan_order(self) -> None:
        h = self.harness()
        h.all_passing()
        result = h.run("--suite", "acceptance", "--check")
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        lines = [line for line in result.stdout.splitlines() if line]
        self.assertEqual([re.split(r"\s{2,}", line)[0] for line in lines[:-1]],
                         [row for row, _, _ in ROWS])
        self.assertTrue(lines[-1].startswith("bench-modules: acceptance --check: 14 rows:"), lines[-1])
        self.assertIn("9 pass, 0 fail, 5 pending, 0 error; exit 3", lines[-1])

    def test_every_row_carries_plans_threshold_verbatim(self) -> None:
        h = self.harness()
        h.all_passing()
        rows = row_lines(h.run("--suite", "acceptance", "--check").stdout)
        for row, target, _ in ROWS:
            with self.subTest(row=row):
                self.assertEqual(rows[row][2], target)

    def test_pending_rows_name_the_missing_component_and_its_stream(self) -> None:
        h = self.harness()
        h.all_passing()
        rows = row_lines(h.run("--suite", "acceptance", "--check").stdout)
        for row, _, owner in ROWS:
            if not owner:
                continue
            with self.subTest(row=row):
                self.assertEqual(rows[row][3].split()[0], "PENDING")
                self.assertIn(f"(owner {owner})", " ".join(rows[row][3:]))
                self.assertRegex(" ".join(rows[row][3:]), r"no .*component|no host module assembly")

    def test_each_case_runs_once_in_its_own_release_test_process(self) -> None:
        h = self.harness()
        h.all_passing()
        h.run("--suite", "acceptance", "--check")
        self.assertEqual(sorted(h.measured_cases()), sorted(case for case, *_ in PASSING.values()))
        for call in h.calls():
            if "--exact" in call:
                self.assertRegex(call, r"^cargo test --locked --release -p p1-module-tests --test acceptance -- "
                                       r"--ignored --nocapture --test-threads=1 --exact \w+$")
        calls = h.calls()
        self.assertEqual(calls[0], "build-modules.sh --all")
        self.assertIn("cargo build --locked --release -p p1-host --bin p1", calls)
        self.assertIn("cargo test --locked --no-fail-fast -p p1-tool-shell --test lead_group_cleanup "
                      "--test process_service --test review", calls)

    def test_a_given_p1_binary_is_not_rebuilt(self) -> None:
        h = self.harness()
        h.reading("cli-startup")
        result = h.run("--suite", "acceptance", "--row", "cli-startup", "--check",
                       P1_BIN=str(h.base / "staged" / "p1"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("cargo build --locked --release -p p1-host --bin p1", h.calls())

    # ---- --row ----------------------------------------------------------------------

    def test_row_selects_only_those_rows_in_table_order_once_each(self) -> None:
        h = self.harness()
        h.all_passing()
        result = h.run("--suite", "acceptance", "--check", "--row", "history-32m",
                       "--row", "history-1m", "--row", "history-32m")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(list(row_lines(result.stdout)), ["history-1m", "history-32m"])
        self.assertEqual(h.measured_cases(), ["history_1m", "history_32m"])
        self.assertNotIn("cargo build --locked --release -p p1-host --bin p1", h.calls())
        self.assertFalse(any("p1-tool-shell" in call for call in h.calls()))

    def test_a_pending_row_alone_builds_and_runs_nothing(self) -> None:
        h = self.harness()
        result = h.run("--suite", "acceptance", "--check", "--row", "compaction-16")
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertEqual(verdicts(result.stdout), {"compaction-16": "PENDING"})
        self.assertEqual(h.calls(), [])

    # ---- thresholds -----------------------------------------------------------------

    def test_each_statistic_form_passes_at_its_limit_and_fails_above_it(self) -> None:
        cases = [
            ("boundary-noop-1k", 1, 1.001),
            ("history-1m", 25, 25.5),
            ("history-32m", 300, 301),
            ("cli-startup", 250, 250.5),
            ("cancel-guest", 100, 100.2),
            ("idle-tool", 8, 8.25),
            ("agents-16-rss", 400, 401),
        ]
        for row, limit, above in cases:
            for value, verdict, code in ((limit, "PASS", 0), (above, "FAIL", 1)):
                with self.subTest(row=row, value=value):
                    h = self.harness()
                    h.reading(row, value=value)
                    result = h.run("--suite", "acceptance", "--check", "--row", row)
                    self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                    self.assertEqual(verdicts(result.stdout), {row: verdict})
                    statistic, unit = PASSING[row][1], PASSING[row][3]
                    self.assertIn(f"{statistic} {value:g} {unit}", row_lines(result.stdout)[row][1])

    def test_a_failure_names_the_value_and_the_limit(self) -> None:
        h = self.harness()
        h.reading("history-1m", value=26.4)
        result = h.run("--suite", "acceptance", "--check", "--row", "history-1m")
        self.assertEqual(result.returncode, 1)
        self.assertIn("FAIL  p95 26.4 ms > 25 ms", result.stdout)

    def test_growth_passes_only_when_none(self) -> None:
        for value, verdict, code in (("none", "PASS", 0), ("continuing (rss)", "FAIL", 1)):
            with self.subTest(value=value):
                h = self.harness()
                h.reading("steady-growth", value=value)
                result = h.run("--suite", "acceptance", "--check", "--row", "steady-growth")
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                self.assertEqual(verdicts(result.stdout), {"steady-growth": verdict})
                self.assertIn(f"growth {value}", result.stdout)

    def test_a_reading_in_another_unit_or_statistic_is_a_tooling_error(self) -> None:
        for kwargs in ({"unit": "us"}, {"statistic": "p50"}, {"value": "fast"}):
            with self.subTest(**kwargs):
                h = self.harness()
                h.reading("history-1m", **kwargs)
                result = h.run("--suite", "acceptance", "--check", "--row", "history-1m")
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertEqual(verdicts(result.stdout), {"history-1m": "ERROR"})

    # ---- classification and exit codes ----------------------------------------------

    def test_exit_0_when_every_selected_row_passes(self) -> None:
        h = self.harness()
        h.all_passing()
        rows = [arg for row in PASSING for arg in ("--row", row)] + ["--row", "cancel-process"]
        result = h.run("--suite", "acceptance", "--check", *rows)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(set(verdicts(result.stdout).values()), {"PASS"})

    def test_exit_1_when_a_row_fails_even_with_pending_rows(self) -> None:
        h = self.harness()
        h.all_passing()
        h.reading("history-32m", value=601.5)
        result = h.run("--suite", "acceptance", "--check")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        found = verdicts(result.stdout)
        self.assertEqual(found["history-32m"], "FAIL")
        self.assertEqual([row for row, verdict in found.items() if verdict == "PENDING"], PENDING)

    def test_exit_3_when_nothing_fails_but_a_row_is_pending(self) -> None:
        h = self.harness()
        h.reading("history-1m")
        result = h.run("--suite", "acceptance", "--check", "--row", "history-1m", "--row", "read-adapter")
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertEqual(verdicts(result.stdout), {"read-adapter": "PENDING", "history-1m": "PASS"})

    def test_exit_2_when_a_case_produces_no_measurement(self) -> None:
        h = self.harness()
        h.all_passing()
        (h.cases / "idle_tool.json").unlink()
        h.reading("history-32m", value=601.5)
        result = h.run("--suite", "acceptance", "--check")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(verdicts(result.stdout)["idle-tool"], "ERROR")
        self.assertIn("case idle_tool failed", result.stdout)
        # The other cases still ran and were classified.
        self.assertEqual(verdicts(result.stdout)["history-32m"], "FAIL")

    def test_exit_2_when_the_cases_do_not_build(self) -> None:
        h = self.harness()
        h.all_passing()
        result = h.run("--suite", "acceptance", "--check", "--row", "history-1m", STUB_BUILD_FAIL="1")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(h.measured_cases(), [])

    def test_a_refusing_local_cargo_config_helper_is_a_tooling_error_not_a_failed_row(self) -> None:
        for row in ("history-1m", "cancel-process"):
            with self.subTest(row=row):
                h = self.harness()
                h.reading("history-1m")
                result = h.run("--suite", "acceptance", "--check", "--row", row,
                               STUB_LOCAL_CARGO_CONFIG_FAIL="1")
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("scripts/local-cargo-config.sh failed", result.stderr)
                self.assertEqual(h.measured_cases(), [])
                self.assertFalse(any("p1-tool-shell" in call for call in h.calls()))

    def test_a_failing_mktemp_is_a_tooling_error(self) -> None:
        h = self.harness()
        h.reading("history-1m")
        result = h.run("--suite", "acceptance", "--check", "--row", "history-1m", STUB_MKTEMP_FAIL="1")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("cannot create a scratch directory", result.stderr)
        self.assertEqual(h.measured_cases(), [])

    def test_a_missing_python3_is_a_tooling_error(self) -> None:
        h = self.harness()
        h.reading("history-1m")
        result = h.run("--suite", "acceptance", "--check", "--row", "history-1m", STUB_PYTHON3_FAIL="1")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("without classifying a row", result.stderr)

    def test_the_process_contract_is_reported_from_its_test_result(self) -> None:
        for contract, verdict, code, value in (
            ("ok", "PASS", 0, "contract 3 passed, 0 failed"),
            ("failed", "FAIL", 1, "contract 1 passed, 1 failed"),
            ("broken", "ERROR", 2, "contract 0 passed, 0 failed"),
        ):
            with self.subTest(contract=contract):
                h = self.harness()
                result = h.run("--suite", "acceptance", "--check", "--row", "cancel-process",
                               STUB_CONTRACT=contract)
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                columns = row_lines(result.stdout)["cancel-process"]
                self.assertEqual(columns[1], value)
                self.assertEqual(columns[3], verdict)

    def test_without_check_rows_are_measured_not_judged(self) -> None:
        h = self.harness()
        h.all_passing()
        h.reading("history-32m", value=601.5)
        result = h.run("--suite", "acceptance")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        found = verdicts(result.stdout)
        self.assertEqual(found["history-32m"], "MEASURED")
        self.assertEqual(found["read-adapter"], "PENDING")
        self.assertNotIn("PASS", found.values())
        self.assertNotIn("FAIL", found.values())

    # ---- output ---------------------------------------------------------------------

    def test_row_lines_carry_value_threshold_verdict_and_reason(self) -> None:
        h = self.harness()
        h.reading("cancel-guest", value=6.07)
        result = h.run("--suite", "acceptance", "--check", "--row", "cancel-guest")
        line = result.stdout.splitlines()[0]
        self.assertRegex(line, r"^cancel-guest\s{2,}p99 6\.07 ms  p99 ≤100 ms after cancellation "
                               r"becomes runnable  PASS  40 samples; stub reading of cancel-guest$")
        self.assertEqual(result.stdout.splitlines()[-1],
                         "bench-modules: acceptance --check: 1 rows: 1 pass, 0 fail, 0 pending, 0 error; exit 0")

    def test_json_holds_every_row_its_verdict_and_the_summary(self) -> None:
        h = self.harness()
        h.all_passing()
        h.reading("history-1m", value=26.381)
        out = h.base / "evidence" / "acceptance.json"
        out.parent.mkdir()
        result = h.run("--suite", "acceptance", "--check", "--json", str(out))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        document = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(document["suite"], "acceptance")
        self.assertTrue(document["check"])
        self.assertEqual(document["exit"], 1)
        self.assertEqual([row["id"] for row in document["rows"]], [row for row, _, _ in ROWS])
        by_id = {row["id"]: row for row in document["rows"]}
        self.assertEqual(by_id["history-1m"]["verdict"], "FAIL")
        self.assertEqual(by_id["history-1m"]["measurements"],
                         [{"statistic": "p95", "value": 26.381, "unit": "ms"}])
        self.assertEqual(by_id["history-1m"]["threshold"], "p95 ≤25 ms")
        self.assertEqual(by_id["history-1m"]["samples"], 40)
        self.assertEqual(by_id["read-adapter"]["verdict"], "PENDING")
        self.assertEqual(by_id["read-adapter"]["owner"], "S2")
        self.assertEqual(by_id["compaction-16"]["owner"], "S5")
        self.assertEqual(document["summary"]["pass"], 8)
        self.assertEqual(document["summary"]["fail"], 1)
        self.assertEqual(document["summary"]["pending"], 5)

    def test_an_unwritable_json_path_is_a_tooling_error(self) -> None:
        h = self.harness()
        h.reading("history-1m")
        result = h.run("--suite", "acceptance", "--check", "--row", "history-1m",
                       "--json", str(h.base / "missing-dir" / "rows.json"))
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("cannot write", result.stderr)


if __name__ == "__main__":
    unittest.main()
