"""Tests for scripts/build-admission.sh: at most three concurrent cargo builds and a 1.2 GiB
MemAvailable floor before a local build starts (owner 2026-10-01, D25).

The process table is a fixture /proc tree (`<pid>/stat`, `<pid>/cmdline`); /proc/meminfo is a
fixture file."""

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
        self.proc = os.path.join(self.dir.name, "proc")
        os.mkdir(self.proc)
        self.meminfo = os.path.join(self.dir.name, "meminfo")

    def process(self, pid: int, ppid: int, argv: list[str], comm: str = "x") -> None:
        here = os.path.join(self.proc, str(pid))
        os.mkdir(here)
        with open(os.path.join(here, "stat"), "w", encoding="utf-8") as handle:
            handle.write(f"{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194304\n")
        with open(os.path.join(here, "cmdline"), "wb") as handle:
            handle.write(b"".join(arg.encode() + b"\0" for arg in argv))

    def run_admission(self, avail_kib: int, timeout: float = 10, **env_extra: str):
        with open(self.meminfo, "w", encoding="utf-8") as handle:
            handle.write(f"MemTotal:       11600000 kB\nMemAvailable:   {avail_kib} kB\n")
        env = dict(os.environ, P1_PROC=self.proc, P1_MEMINFO=self.meminfo,
                   P1_ADMISSION_INTERVAL="0.2")
        env.update(env_extra)
        return subprocess.run(["timeout", str(timeout), "bash", SCRIPT], env=env,
                              capture_output=True, text=True, check=False)

    def test_two_builds_and_enough_memory_start_at_once(self) -> None:
        self.process(10, 1, [CARGO, "test", "-p", "p1-host"])
        self.process(20, 1, [CARGO, "clippy", "--workspace"])
        done = self.run_admission(4_000_000)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stderr, "")

    def test_three_builds_make_it_wait_and_say_why(self) -> None:
        for pid in (10, 20, 30):
            self.process(pid, 1, [CARGO, "build"])
        done = self.run_admission(4_000_000, timeout=1.5)
        self.assertEqual(done.returncode, 124)  # still waiting when timeout ends it
        self.assertEqual(done.stderr.count("build-admission: waiting:"), 1)
        self.assertIn("3 cargo builds running (at most 3)", done.stderr)

    def test_memory_under_the_floor_makes_it_wait(self) -> None:
        done = self.run_admission(1_000_000, timeout=1.5)
        self.assertEqual(done.returncode, 124)
        self.assertIn("MemAvailable 976 MiB (floor 1228 MiB)", done.stderr)

    def test_a_toolchain_override_and_a_path_with_spaces_count(self) -> None:
        self.process(10, 1, ["/home/u/.cargo/bin/cargo", "+stable", "test"])
        self.process(20, 1, ["/opt/rust tools/bin/cargo", "test"], comm="cargo")
        self.process(30, 1, [CARGO, "check"])
        done = self.run_admission(4_000_000, timeout=1.5)
        self.assertEqual(done.returncode, 124)
        self.assertIn("3 cargo builds running", done.stderr)

    def test_nested_cargo_and_non_build_commands_do_not_count(self) -> None:
        self.process(10, 1, [CARGO, "test", "-p", "p1-host"])
        self.process(11, 10, [CARGO, "build", "--bin", "p1"])        # started by build 10
        chain = 10
        for pid in range(100, 180):                                   # 80 levels below build 10
            self.process(pid, chain, ["/tmp/t/target/debug/deps/installed_release-0a1b"])
            chain = pid
        self.process(200, chain, [CARGO, "build", "--manifest-path", "modules/Cargo.toml"])
        self.process(20, 1, [CARGO, "metadata", "--no-deps"])         # no compile
        self.process(30, 1, [CARGO, "fmt", "--all"])                  # no compile
        self.process(40, 1, ["/usr/bin/vim", "cargo", "build"])       # not cargo
        self.process(50, 1, [CARGO, "check", "-p", "p1-core"])
        done = self.run_admission(4_000_000)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_a_parent_cycle_ends_the_walk(self) -> None:
        self.process(10, 11, [CARGO, "test"])
        self.process(11, 10, ["/bin/sh"])
        done = self.run_admission(4_000_000)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_a_bad_interval_is_refused(self) -> None:
        done = self.run_admission(4_000_000, P1_ADMISSION_INTERVAL="bad")
        self.assertEqual(done.returncode, 2)
        self.assertIn("must be a number of seconds", done.stderr)


if __name__ == "__main__":
    unittest.main()
