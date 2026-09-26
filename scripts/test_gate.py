#!/usr/bin/env python3
"""Tests for scripts/gate.sh — stdlib only.

The gate runs in a temporary repository whose scripts/ holds stub helpers and whose PATH holds
stub cargo, wasm-tools, bwrap and timeout. Every stub appends one line to a call log and fails
when that line matches the STUB_FAIL regular expression, so each case can break exactly one
step. Real cargo never runs.
"""
from __future__ import annotations

import os
import pathlib
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
GATE = ROOT / "scripts" / "gate.sh"
PACKAGE = "p1-module-demo"

# Logs its own name and arguments, then fails when that line matches STUB_FAIL.
LOG_AND_FAIL = textwrap.dedent(
    """\
    line="$(basename "$0")${*:+ $*}"
    printf '%s\\n' "$line" >> "$STUB_LOG"
    if [ -n "${STUB_FAIL:-}" ] && [[ "$line" =~ $STUB_FAIL ]]; then
      echo "stub: failing $line" >&2
      exit 1
    fi
    """
)

CARGO_STUB = "#!/usr/bin/env bash\n" + LOG_AND_FAIL + textwrap.dedent(
    """\
    if [ "${1:-}" = metadata ]; then
      printf '{"target_directory":"%s/target"}\\n' "$PWD"
    fi
    exit 0
    """
)

# `component wit` prints a world derived from the component's bytes, so a changed component
# no longer matches the recorded world.
WASM_TOOLS_STUB = "#!/usr/bin/env bash\n" + LOG_AND_FAIL + textwrap.dedent(
    """\
    if [ "${1:-}" = component ] && [ "${2:-}" = wit ]; then
      printf 'world demo-%s {}\\n' "$(sha256sum "$3" | cut -c1-12)"
    fi
    exit 0
    """
)

BWRAP_STUB = "#!/usr/bin/env bash\n" + LOG_AND_FAIL + "exit 0\n"

# Records the guard, then runs the guarded command.
TIMEOUT_STUB = "#!/usr/bin/env bash\n" + LOG_AND_FAIL + textwrap.dedent(
    """\
    shift 2
    exec "$@"
    """
)

HELPER_STUB = "#!/usr/bin/env bash\n" + LOG_AND_FAIL + "exit 0\n"

# The build stub writes the five build outputs of package.md, then damages them on request.
BUILD_STUB = "#!/usr/bin/env bash\n" + LOG_AND_FAIL + textwrap.dedent(
    f"""\
    pkg={PACKAGE}
    out="modules/target/p1-modules/$pkg"
    mkdir -p "$out"
    printf 'component bytes' > "$out/$pkg.wasm"
    wasm-tools component wit "$out/$pkg.wasm" > "$out/$pkg.wit"
    (cd "$out" && sha256sum "$pkg.wasm" > "$pkg.sha256")
    printf 'p1:module/control@1.0.0\\n' > "$out/$pkg.imports"
    digest="$(cut -d' ' -f1 "$out/$pkg.sha256")"
    printf '{{\\n  "name": "p1/demo",\\n  "digest": "sha256:%s",\\n  "size": 15\\n}}\\n' "$digest" > "$out/$pkg.manifest.json"
    case "${{STUB_DAMAGE:-}}" in
      digest) printf 'other bytes' > "$out/$pkg.wasm.tmp"; mv "$out/$pkg.wasm.tmp" "$out/$pkg.wasm"
              wasm-tools component wit "$out/$pkg.wasm" > "$out/$pkg.wit" ;;
      wit) printf 'world other {{}}\\n' > "$out/$pkg.wit" ;;
      manifest) sed -i 's/sha256:[0-9a-f]*/sha256:0000/' "$out/$pkg.manifest.json" ;;
      missing) rm "$out/$pkg.imports" ;;
      extra) mkdir -p modules/target/p1-modules/p1-module-gone ;;
    esac
    exit 0
    """
)

PY_STUB = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import os, re, sys
    line = " ".join([os.path.basename(sys.argv[0])] + sys.argv[1:])
    with open(os.environ["STUB_LOG"], "a", encoding="utf-8") as log:
        log.write(line + "\\n")
    pattern = os.environ.get("STUB_FAIL", "")
    sys.exit(1 if pattern and re.search(pattern, line) else 0)
    """
)

PY_TESTS = [
    "test_adr.py",
    "test_ci_build.py",
    "test_fanout.py",
    "test_gate.py",
    "test_install.py",
    "test_local_cargo_config.py",
    "test_release_manifest.py",
    "test_run_report.py",
    "test_rustc_serial.py",
    "test_secret_scan.py",
    "test_usage_audit.py",
]

# The expected call order of an all-green gate, one regular expression per logged call.
ORDER = [
    ("native fmt", r"^cargo fmt --all -- --check$"),
    ("guest fmt", r"^cargo fmt --manifest-path modules/Cargo\.toml --all -- --check$"),
    ("native clippy", r"^cargo clippy --workspace --all-targets --locked -- -D warnings$"),
    ("toolchain", r"^module-toolchain\.sh --check$"),
    ("guest check", r"^cargo clippy --manifest-path modules/Cargo\.toml --workspace --locked --target wasm32-unknown-unknown -- -D warnings$"),
    ("module build", r"^build-modules\.sh --all$"),
    ("module validation", r"^wasm-tools validate "),
    ("module boundary", r"^check-module-boundaries\.sh$"),
    ("bwrap probe", r"^bwrap --ro-bind / / --dev /dev --proc /proc true$"),
    ("test guard", r"^timeout --foreground \d+ cargo test --workspace --locked$"),
    ("tests", r"^cargo test --workspace --locked$"),
    ("core isolation", r"^check-core-isolation\.sh$"),
    ("secret scan", r"^secret-scan\.sh$"),
    ("adr", r"^adr\.py check$"),
] + [(name, "^" + re.escape(name) + " -q$") for name in PY_TESTS]


def write_exec(path: pathlib.Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class Harness:
    def __init__(self, pins: str = "RUST_MIN=1.96.0\nWASM_TARGET=wasm32-unknown-unknown\n") -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="gate-test-")
        base = pathlib.Path(self.tmp.name)
        self.repo = base / "repo"
        self.log = base / "calls.log"
        bin_dir = base / "bin"
        scripts = self.repo / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(GATE, scripts / "gate.sh")
        for name in ("local-cargo-config.sh", "module-toolchain.sh", "check-core-isolation.sh",
                     "check-module-boundaries.sh", "secret-scan.sh"):
            write_exec(scripts / name, HELPER_STUB)
        write_exec(scripts / "build-modules.sh", BUILD_STUB)
        write_exec(scripts / "adr.py", PY_STUB)
        for name in PY_TESTS:
            write_exec(scripts / name, PY_STUB)
        (self.repo / "target").mkdir()
        (self.repo / ".cargo").mkdir()
        (self.repo / ".cargo" / "config.toml").write_text("", encoding="utf-8")
        (self.repo / "modules" / PACKAGE).mkdir(parents=True)
        (self.repo / "modules" / PACKAGE / "Cargo.toml").write_text(
            '[package]\nname = "p1-module-demo"\n\n[package.metadata.p1-module]\nname = "p1/demo"\n',
            encoding="utf-8",
        )
        (self.repo / "modules" / "p1-bindings-demo").mkdir()
        (self.repo / "modules" / "p1-bindings-demo" / "Cargo.toml").write_text(
            '[package]\nname = "p1-bindings-demo"\n', encoding="utf-8"
        )
        (self.repo / "modules" / "toolchain.pins").write_text(pins, encoding="utf-8")
        write_exec(bin_dir / "cargo", CARGO_STUB)
        write_exec(bin_dir / "wasm-tools", WASM_TOOLS_STUB)
        write_exec(bin_dir / "bwrap", BWRAP_STUB)
        write_exec(bin_dir / "timeout", TIMEOUT_STUB)
        self.env = {
            "PATH": f"{bin_dir}:/usr/bin:/bin",
            "HOME": str(base),
            "STUB_LOG": str(self.log),
            "LC_ALL": "C",
        }

    def run(self, **extra: str) -> subprocess.CompletedProcess[str]:
        env = dict(self.env)
        env.update(extra)
        return subprocess.run(
            ["bash", str(self.repo / "scripts" / "gate.sh")],
            cwd=self.repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )

    def calls(self) -> list[str]:
        if not self.log.exists():
            return []
        return self.log.read_text(encoding="utf-8").splitlines()

    def steps(self) -> list[str]:
        """The ORDER names of the logged calls, in call order, each named once."""
        seen: list[str] = []
        for call in self.calls():
            for name, pattern in ORDER:
                if re.search(pattern, call) and name not in seen:
                    seen.append(name)
                    break
        return seen

    def cleanup(self) -> None:
        self.tmp.cleanup()


class GateTests(unittest.TestCase):
    def harness(self, **kwargs) -> Harness:
        h = Harness(**kwargs)
        self.addCleanup(h.cleanup)
        return h

    def assert_red(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("== gate: GREEN", result.stdout)

    def test_green_run_calls_every_step_in_order(self) -> None:
        h = self.harness()
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.splitlines()[-1], "== gate: GREEN")
        self.assertEqual(h.steps(), [name for name, _ in ORDER])
        self.assertIn("module validation: 1 package(s) valid", result.stdout)

    def test_modules_are_built_and_validated_before_the_tests(self) -> None:
        h = self.harness()
        h.run()
        steps = h.steps()
        self.assertLess(steps.index("module build"), steps.index("tests"))
        self.assertLess(steps.index("module validation"), steps.index("tests"))
        self.assertLess(steps.index("module boundary"), steps.index("tests"))

    def test_every_step_failing_stops_the_gate_red(self) -> None:
        names = [name for name, _ in ORDER]
        for index, (name, pattern) in enumerate(ORDER):
            if name == "test guard":
                continue  # the guard only runs the tests; "tests" covers a red test run
            with self.subTest(step=name):
                h = self.harness()
                result = h.run(STUB_FAIL=pattern)
                self.assert_red(result)
                ran = h.steps()
                self.assertEqual(ran[-1], name, ran)
                self.assertEqual(ran, names[: index + 1])

    def test_the_gate_reads_no_variable_that_could_turn_a_step_off(self) -> None:
        # CI only relaxes the bubblewrap probe; every other variable the gate reads is a
        # build setting. A new switch would have to be added here, in review.
        text = GATE.read_text(encoding="utf-8")
        read = set(re.findall(r"\$\{?([A-Z][A-Z0-9_]*)", text))
        self.assertEqual(read, {"CI", "CARGO_BUILD_JOBS", "CARGO_TERM_COLOR"})

    def test_a_guest_failure_is_red_on_ci_too(self) -> None:
        for step in ("guest fmt", "guest check", "module build", "module validation", "module boundary"):
            with self.subTest(step=step):
                h = self.harness()
                result = h.run(CI="true", STUB_FAIL=dict(ORDER)[step])
                self.assert_red(result)
                self.assertNotIn("tests", h.steps())

    def test_damaged_build_outputs_fail_module_validation(self) -> None:
        for damage, message in (
            ("digest", "does not match"),
            ("wit", "is not the world of"),
            ("manifest", "names another digest"),
            ("missing", ".imports is missing"),
            ("extra", "differ from the packages under modules/"),
        ):
            with self.subTest(damage=damage):
                h = self.harness()
                result = h.run(STUB_DAMAGE=damage)
                self.assert_red(result)
                self.assertIn(message, result.stderr)
                self.assertNotIn("tests", h.steps())

    def test_missing_build_outputs_fail_module_validation(self) -> None:
        h = self.harness()
        # The build "succeeds" but writes nothing: its stub output is removed first.
        write_exec(h.repo / "scripts" / "build-modules.sh", HELPER_STUB)
        result = h.run()
        self.assert_red(result)
        self.assertIn("differ from the packages under modules/", result.stderr)

    def test_unusable_bwrap_is_red_outside_ci(self) -> None:
        h = self.harness()
        result = h.run(STUB_FAIL=r"^bwrap ")
        self.assert_red(result)
        self.assertIn("bwrap is unusable", result.stderr)
        self.assertNotIn("tests", h.steps())

    def test_unusable_bwrap_is_allowed_on_ci(self) -> None:
        h = self.harness()
        result = h.run(CI="true", STUB_FAIL=r"^bwrap ")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("boundary tests skip (ADR-0077)", result.stdout)
        self.assertEqual(result.stdout.splitlines()[-1], "== gate: GREEN")

    def test_guest_check_uses_the_pinned_target(self) -> None:
        h = self.harness(pins="# pins\nWASM_TARGET=wasm32-example\n")
        result = h.run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        guest = [c for c in h.calls() if c.startswith("cargo clippy --manifest-path modules/Cargo.toml")]
        self.assertEqual(len(guest), 1, h.calls())
        self.assertIn("--target wasm32-example ", guest[0])

    def test_pins_are_parsed_not_executed(self) -> None:
        h = self.harness(pins="WASM_TARGET=wasm32-wasip2\n$(touch pwned)\n")
        h.run()
        self.assertFalse((h.repo / "pwned").exists())

    def test_missing_target_pin_is_red_before_any_step(self) -> None:
        h = self.harness(pins="RUST_MIN=1.96.0\n")
        result = h.run()
        self.assert_red(result)
        self.assertIn("WASM_TARGET is missing", result.stderr)
        self.assertEqual(h.calls(), [])


if __name__ == "__main__":
    sys.exit(unittest.main())
