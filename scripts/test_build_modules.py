#!/usr/bin/env python3
"""Tests for scripts/build-modules.sh — stdlib only, stub tools on PATH.

The script under test runs in a temporary repository that holds the frozen data it reads
(modules/toolchain.pins, modules/capabilities.toml and one fixture package) with stub `cargo`,
`wasm-tools` and `python3` first on PATH: no real toolchain, no network and no user directory is
touched. The stub cargo records the RUSTFLAGS (or CARGO_ENCODED_RUSTFLAGS) the guest build sees,
so the path remaps the frozen case S7-N7 requires are checked against a repository whose paths
this file owns.
"""
from __future__ import annotations

import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "build-modules.sh"

PACKAGE = "p1-module-fixture"

PACKAGE_MANIFEST = textwrap.dedent(
    f"""\
    [package]
    name = "{PACKAGE}"
    version = "0.0.1"
    edition = "2024"

    [package.metadata.p1-module]
    name = "p1/fixture"
    kind = "tool"
    world = "p1:module/tool@1.0.0"
    protocol = "1.0"
    capabilities = ["control"]
    variant = "default"
    """
)

CAPABILITIES = """\
type-only = ["types"]

[tool]
imports = ["control"]
"""

# The stub cargo answers `metadata` with the target directory the test chose, and on `build`
# writes the guest's core module and records the rustflags the build saw for the test to read.
CARGO_STUB = """\
#!/usr/bin/env bash
printf '%s\\n' "cargo${*:+ $*}" >> "$STUB_LOG"
if [ "${1:-}" = metadata ]; then
  printf '{"target_directory":"%s"}\\n' "$STUB_TARGET_DIR"
  exit 0
fi
if [ "${1:-}" = build ]; then
  {
    printf 'RUSTFLAGS=%s\\n' "${RUSTFLAGS-<unset>}"
    printf 'CARGO_ENCODED_RUSTFLAGS=%s\\n' "${CARGO_ENCODED_RUSTFLAGS-<unset>}"
  } > "$STUB_BUILD_ENV"
  target=""; pkg=""; prev=""
  for arg in "$@"; do
    case "$prev" in
      --target) target="$arg" ;;
      -p) pkg="$arg" ;;
    esac
    prev="$arg"
  done
  core="$STUB_TARGET_DIR/$target/release/${pkg//-/_}.wasm"
  mkdir -p "$(dirname "$core")"
  printf '\\000asm\\001\\000\\000\\000core of %s\\n' "$pkg" > "$core"
fi
exit 0
"""

# The stub wasm-tools writes the component the build script publishes, and prints a fixed world
# so the extracted .wit is stable.
WASM_TOOLS_STUB = """\
#!/usr/bin/env bash
printf '%s\\n' "wasm-tools${*:+ $*}" >> "$STUB_LOG"
if [ "${1:-}" = component ] && [ "${2:-}" = new ]; then
  out=""; prev=""
  for arg in "$@"; do
    [ "$prev" = -o ] && out="$arg"
    prev="$arg"
  done
  printf 'component of %s\\n' "$3" > "$out"
elif [ "${1:-}" = component ] && [ "${2:-}" = wit ]; then
  printf 'world fixture {}\\n'
fi
exit 0
"""

PYTHON_STUB = """\
#!/usr/bin/env bash
printf '%s\\n' "python3${*:+ $*}" >> "$STUB_LOG"
exit 0
"""


def write_exec(path: pathlib.Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class Harness:
    """A temporary repository with the fixture, a stub cargo, wasm-tools and python3."""

    def __init__(self, target_dir: str | None = None) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="build-modules-test-")
        self.base = pathlib.Path(self.tmp.name)
        self.repo = self.base / "repo"
        self.bin = self.base / "bin"
        self.log = self.base / "calls.log"
        self.build_log = self.base / "build-env"
        # The default target directory lies inside the checkout, so no target-dir remap is due.
        self.target_dir = target_dir or str(self.repo / "target")
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.repo / "scripts" / "build-modules.sh")
        (self.repo / "modules" / PACKAGE).mkdir(parents=True)
        (self.repo / "modules" / PACKAGE / "Cargo.toml").write_text(
            PACKAGE_MANIFEST, encoding="utf-8"
        )
        (self.repo / "modules" / "toolchain.pins").write_text(
            "WASM_TARGET=wasm32-unknown-unknown\n", encoding="utf-8"
        )
        (self.repo / "modules" / "capabilities.toml").write_text(CAPABILITIES, encoding="utf-8")
        write_exec(self.bin / "cargo", CARGO_STUB)
        write_exec(self.bin / "wasm-tools", WASM_TOOLS_STUB)
        write_exec(self.bin / "python3", PYTHON_STUB)

    def run(self, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        environment = {
            "PATH": os.pathsep.join([str(self.bin), "/usr/bin", "/bin"]),
            "HOME": str(self.base),
            "LC_ALL": "C",
            "STUB_LOG": str(self.log),
            "STUB_BUILD_ENV": str(self.build_log),
            "STUB_TARGET_DIR": self.target_dir,
        }
        environment.update(env)
        return subprocess.run(
            ["bash", str(self.repo / "scripts" / "build-modules.sh"), *args],
            cwd=self.repo,
            env=environment,
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )

    def build_env(self) -> dict[str, str]:
        """The RUSTFLAGS and CARGO_ENCODED_RUSTFLAGS the stub cargo's build call saw."""
        values: dict[str, str] = {}
        if not self.build_log.exists():
            return values
        for line in self.build_log.read_text(encoding="utf-8").splitlines():
            key, _, value = line.partition("=")
            values[key] = value
        return values

    def cargo_home(self) -> str:
        return str(self.base / ".cargo")

    def root(self) -> str:
        # build-modules.sh remaps `pwd -P`, so the comparison uses the physical checkout path.
        return str(self.repo.resolve())

    def cleanup(self) -> None:
        self.tmp.cleanup()


class RemapTests(unittest.TestCase):
    def harness(self, target_dir: str | None = None) -> Harness:
        h = Harness(target_dir)
        self.addCleanup(h.cleanup)
        return h

    def test_the_guest_build_remaps_the_default_cargo_home_and_the_root(self) -> None:
        h = self.harness()
        result = h.run("--package", PACKAGE)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            h.build_env().get("RUSTFLAGS"),
            f"--remap-path-prefix={h.cargo_home()}=/cargo"
            f" --remap-path-prefix={h.root()}=/p1",
        )
        self.assertEqual(h.build_env().get("CARGO_ENCODED_RUSTFLAGS"), "<unset>")

    def test_the_default_cargo_home_is_home_cargo(self) -> None:
        h = self.harness()
        self.assertEqual(h.cargo_home(), str(h.base / ".cargo"))
        h.run("--package", PACKAGE)
        self.assertIn(f"--remap-path-prefix={h.base}/.cargo=/cargo", h.build_env()["RUSTFLAGS"])

    def test_a_cargo_home_from_the_environment_is_remapped(self) -> None:
        h = self.harness()
        result = h.run("--package", PACKAGE, CARGO_HOME="/opt/cargo-home")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        flags = h.build_env()["RUSTFLAGS"]
        self.assertIn("--remap-path-prefix=/opt/cargo-home=/cargo", flags)
        self.assertNotIn(f"--remap-path-prefix={h.cargo_home()}=", flags)

    def test_a_target_dir_outside_the_root_is_remapped(self) -> None:
        h = self.harness()
        # A target directory under the test's temporary root but outside the checkout it builds.
        target = str(h.base / "elsewhere" / "target")
        result = h.run("--package", PACKAGE, STUB_TARGET_DIR=target)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"--remap-path-prefix={target}=/p1-target", h.build_env()["RUSTFLAGS"])

    def test_a_target_dir_inside_the_root_is_not_remapped(self) -> None:
        h = self.harness()
        h.run("--package", PACKAGE)
        self.assertNotIn("/p1-target", h.build_env()["RUSTFLAGS"])

    def test_a_callers_rustflags_survives(self) -> None:
        h = self.harness()
        caller = "-C debuginfo=1 --cfg feature=\"extra\""
        result = h.run("--package", PACKAGE, RUSTFLAGS=caller)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            h.build_env()["RUSTFLAGS"],
            f"{caller} --remap-path-prefix={h.cargo_home()}=/cargo"
            f" --remap-path-prefix={h.root()}=/p1",
        )

    def test_encoded_rustflags_are_extended_not_overridden(self) -> None:
        h = self.harness()
        sep = "\x1f"
        caller = f"-C{sep}debuginfo=1"
        result = h.run("--package", PACKAGE, CARGO_ENCODED_RUSTFLAGS=caller)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            h.build_env()["CARGO_ENCODED_RUSTFLAGS"],
            caller
            + f"{sep}--remap-path-prefix={h.cargo_home()}=/cargo"
            + f"{sep}--remap-path-prefix={h.root()}=/p1",
        )
        # Cargo ignores RUSTFLAGS when the encoded form is set, so the build must not carry one.
        self.assertEqual(h.build_env()["RUSTFLAGS"], "<unset>")

    def test_the_fixture_package_builds_and_publishes_its_outputs(self) -> None:
        h = self.harness()
        result = h.run("--package", PACKAGE)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        out = h.repo / "modules" / "target" / "p1-modules" / PACKAGE
        for name in (
            f"{PACKAGE}.wasm",
            f"{PACKAGE}.wit",
            f"{PACKAGE}.sha256",
            f"{PACKAGE}.imports",
            f"{PACKAGE}.manifest.json",
        ):
            with self.subTest(name=name):
                self.assertTrue((out / name).is_file(), name)
        self.assertRegex(result.stdout, r"(?m)^build-modules: p1-module-fixture ok sha256:[0-9a-f]{64} ")
        self.assertTrue(
            result.stdout.splitlines()[-1].startswith("build-modules: p1-module-fixture ok "),
            result.stdout,
        )


if __name__ == "__main__":
    sys.exit(unittest.main())
