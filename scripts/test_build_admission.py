"""Tests for scripts/build-admission.sh: at most three concurrent cargo builds and a 1.2 GiB
MemAvailable floor before a local build starts (owner 2026-10-01, D25).

`ps` is a stub that prints a fixture process table; /proc/meminfo is a fixture file."""

import os
import subprocess
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "scripts", "build-admission.sh")
CARGO = "/home/u/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo"


class BuildAdmissionTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory(prefix="build-admission-test-")
        self.addCleanup(self.dir.cleanup)
        self.bin = os.path.join(self.dir.name, "bin")
        os.mkdir(self.bin)
        self.table = os.path.join(self.dir.name, "ps.txt")
        stub = os.path.join(self.bin, "ps")
        with open(stub, "w", encoding="utf-8") as handle:
            handle.write('#!/bin/sh\ncat "$PS_FIXTURE"\n')
        os.chmod(stub, 0o755)
        self.meminfo = os.path.join(self.dir.name, "meminfo")

    def run_admission(self, processes: list[str], avail_kib: int, timeout: float = 10):
        with open(self.table, "w", encoding="utf-8") as handle:
            handle.write("".join(line + "\n" for line in processes))
        with open(self.meminfo, "w", encoding="utf-8") as handle:
            handle.write(f"MemTotal:       11600000 kB\nMemAvailable:   {avail_kib} kB\n")
        env = dict(os.environ, PATH=self.bin + os.pathsep + os.environ["PATH"],
                   PS_FIXTURE=self.table, P1_MEMINFO=self.meminfo, P1_ADMISSION_INTERVAL="0.2")
        return subprocess.run(["timeout", str(timeout), "bash", SCRIPT], env=env,
                              capture_output=True, text=True, check=False)

    def test_two_builds_and_enough_memory_start_at_once(self) -> None:
        done = self.run_admission([f"10 1 {CARGO} test -p p1-host", f"20 1 {CARGO} clippy --workspace"],
                                  4_000_000)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stderr, "")

    def test_three_builds_make_it_wait_and_say_why(self) -> None:
        table = [f"{pid} 1 {CARGO} build" for pid in (10, 20, 30)]
        done = self.run_admission(table, 4_000_000, timeout=1.5)
        self.assertEqual(done.returncode, 124)  # still waiting when timeout ends it
        self.assertEqual(done.stderr.count("build-admission: waiting:"), 1)
        self.assertIn("3 cargo builds running (at most 3)", done.stderr)

    def test_memory_under_the_floor_makes_it_wait(self) -> None:
        done = self.run_admission([], 1_000_000, timeout=1.5)
        self.assertEqual(done.returncode, 124)
        self.assertIn("MemAvailable 976 MiB (floor 1228 MiB)", done.stderr)

    def test_child_cargo_and_non_build_commands_do_not_count(self) -> None:
        table = [
            f"10 1 {CARGO} test -p p1-host",
            f"11 10 {CARGO} build --bin p1",       # started by build 10: the same build
            "12 10 /tmp/t/target/debug/deps/installed_release-0a1b",
            f"13 12 {CARGO} build --manifest-path modules/Cargo.toml",  # below build 10's test
            f"20 1 {CARGO} metadata --no-deps",    # no compile
            f"30 1 {CARGO} fmt --all",             # no compile
            "40 1 /usr/bin/vim notes-on-cargo build",
            f"50 1 {CARGO} check -p p1-core",
        ]
        done = self.run_admission(table, 4_000_000)
        self.assertEqual(done.returncode, 0, done.stderr)


if __name__ == "__main__":
    unittest.main()
