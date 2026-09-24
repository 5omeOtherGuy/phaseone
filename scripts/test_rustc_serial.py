#!/usr/bin/env python3
"""Unit tests for scripts/rustc-serial — stdlib unittest, no cargo.

    python3 scripts/test_rustc_serial.py [-q]

Every test gets its own temporary `P1_BUILD_LOCK_DIR`; the machine's live lock
directory is never named or touched. rustc is a fake shell script that records
its argv/pid, optionally blocks on a file the test controls, and exits with a
chosen code. Holders are synthetic: either a real process started by the test
(which the test may SIGSTOP/SIGCONT and must always resume and reap) or an
flock held directly from this process. Synchronisation is by small files plus
bounded polling; no fixed sleep is used as an assertion.
"""
from __future__ import annotations

import contextlib
import fcntl
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
WRAPPER = os.path.join(HERE, "rustc-serial")

# Fake rustc: records pid + argv into <FAKE_DIR>/argv.<pid>, publishes an
# <FAKE_DIR>/active.<pid> marker while it runs, optionally blocks until
# FAKE_RELEASE exists, then exits with FAKE_EXIT.
FAKE_RUSTC = """#!/bin/sh
dir="${FAKE_DIR:?}"
{
  printf '%s\\n' "$$"
  for a in "$@"; do printf '%s\\n' "$a"; done
} > "$dir/argv.$$"
: > "$dir/active.$$"
if [ -n "${FAKE_RELEASE:-}" ]; then
  while [ ! -e "$FAKE_RELEASE" ]; do sleep 0.01; done
fi
rm -f "$dir/active.$$"
exit "${FAKE_EXIT:-0}"
"""

# Synthetic holder: takes the slot's flock, announces readiness, and holds it
# until HOLDER_RELEASE exists. `exec` keeps its pid, so the test can SIGSTOP it.
HOLDER = """#!/bin/bash
exec {fd}>>"$HOLDER_SLOT"
if ! flock "$fd"; then exit 1; fi
: > "$HOLDER_READY"
while [ ! -e "$HOLDER_RELEASE" ]; do sleep 0.02; done
"""

# Helper process for the inherited-descriptor case: locks the slot, forks a
# child that keeps the same open file description (and thus the lock), prints
# the child pid and exits, leaving the lock held by a process whose recorded
# pid is dead.
INHERIT_HELPER = r"""
import fcntl, os, sys, time
fd = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o600)
fcntl.flock(fd, fcntl.LOCK_EX)
pid = os.fork()
if pid == 0:
    os.close(0)
    os.close(1)
    os.close(2)
    time.sleep(300)
    os._exit(0)
sys.stdout.write(str(pid))
sys.stdout.flush()
"""


def read_boot_id() -> str:
    with open("/proc/sys/kernel/random/boot_id", encoding="ascii") as handle:
        return handle.read().strip()


def proc_fields(pid: int) -> list[str]:
    """Fields of /proc/<pid>/stat after the final ')' (state is field 0)."""
    with open(f"/proc/{pid}/stat", encoding="ascii", errors="replace") as handle:
        text = handle.read()
    rest = text.rsplit(")", 1)[1] if ")" in text else text
    return rest.split()


def proc_start_ticks(pid: int) -> str:
    return proc_fields(pid)[19]


def proc_state(pid: int) -> str:
    return proc_fields(pid)[0]


def write_executable(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    os.chmod(path, 0o755)


class Run:
    """One rustc-serial invocation under test."""

    def __init__(self, popen: subprocess.Popen, run_dir: str, errfile, errpath: str):
        self.popen = popen
        self.run_dir = run_dir
        self.errfile = errfile
        self.errpath = errpath

    @property
    def pid(self) -> int:
        return self.popen.pid

    def alive(self) -> bool:
        return self.popen.poll() is None

    def stderr(self) -> str:
        with open(self.errpath, encoding="utf-8", errors="replace") as handle:
            return handle.read()

    def argv_records(self) -> list[list[str]]:
        records = []
        for name in sorted(os.listdir(self.run_dir)):
            if name.startswith("argv."):
                with open(os.path.join(self.run_dir, name), encoding="utf-8") as handle:
                    records.append(handle.read().splitlines())
        return records

    def active(self) -> int:
        return sum(1 for n in os.listdir(self.run_dir) if n.startswith("active."))

    def wait(self, timeout: float = 30.0) -> int:
        code = self.popen.wait(timeout=timeout)
        with contextlib.suppress(OSError):
            self.errfile.close()
        return code


class RustcSerialTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="rustc-serial-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.locks = os.path.join(self.dir, "locks")
        self.fake = os.path.join(self.dir, "fake-rustc")
        write_executable(self.fake, FAKE_RUSTC)
        self.holder = os.path.join(self.dir, "holder")
        write_executable(self.holder, HOLDER)
        self.fake_release = os.path.join(self.dir, "fake-release")
        self.allow_fake()                     # default: fake exits at once
        self._procs: list[subprocess.Popen] = []
        self._fds: list[int] = []
        self._orphans: list[int] = []
        self._errfiles: list = []
        self.addCleanup(self._cleanup)

    # --- cleanup -----------------------------------------------------------

    def _cleanup(self) -> None:
        for fd in self._fds:
            with contextlib.suppress(OSError):
                os.close(fd)
        for proc in self._procs:
            self._reap(proc)
        for errfile in self._errfiles:
            with contextlib.suppress(OSError):
                errfile.close()
        for pid in self._orphans:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)

    @staticmethod
    def _reap(proc: subprocess.Popen) -> None:
        if proc.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.kill(proc.pid, signal.SIGCONT)
            with contextlib.suppress(ProcessLookupError):
                os.kill(proc.pid, signal.SIGKILL)
        with contextlib.suppress(Exception):
            proc.wait(timeout=10)

    # --- small helpers -----------------------------------------------------

    def allow_fake(self) -> None:
        with open(self.fake_release, "w", encoding="utf-8"):
            pass

    def block_fake(self) -> None:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(self.fake_release)

    def wait_until(self, pred, timeout: float = 20.0, interval: float = 0.02) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if pred():
                return True
            time.sleep(interval)
        return pred()

    def assert_no_admission(self, run: "Run", seconds: float = 1.0) -> None:
        """Poll for a bounded window: the waiter must not admit a rustc."""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.assertEqual(run.argv_records(), [], run.stderr())
            self.assertTrue(run.alive(), run.stderr())
            time.sleep(0.02)

    def spawn(self, *args: str, slots=2, lock_dir=None, extra=None,
              run_dir=None, stderr_path=None) -> Run:
        run_dir = run_dir or tempfile.mkdtemp(prefix="run-", dir=self.dir)
        env = dict(os.environ)
        env["P1_BUILD_LOCK_DIR"] = lock_dir or self.locks
        env["P1_RUSTC_SLOTS"] = str(slots)
        env["FAKE_DIR"] = run_dir
        env["FAKE_RELEASE"] = self.fake_release
        if extra:
            env.update(extra)
        errpath = stderr_path or os.path.join(run_dir, "stderr")
        errfile = open(errpath, "wb")
        popen = subprocess.Popen(["bash", WRAPPER, self.fake, *args], env=env,
                                 stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=errfile)
        self._procs.append(popen)
        self._errfiles.append(errfile)
        return Run(popen, run_dir, errfile, errpath)

    def run_ok(self, *args: str, slots=2, **kwargs) -> Run:
        run = self.spawn(*args, slots=slots, **kwargs)
        self.assertEqual(run.wait(), 0, run.stderr())
        return run

    def start_holder(self, slot: int, crate: str = "held") -> tuple[subprocess.Popen, str, str, int]:
        """Start a synthetic holder on `slot`; return (proc, ready, release, start_ticks)."""
        os.makedirs(self.locks, mode=0o700, exist_ok=True)
        ready = os.path.join(self.dir, f"ready-{slot}")
        release = os.path.join(self.dir, f"holder-release-{slot}")
        env = dict(os.environ)
        env["HOLDER_SLOT"] = os.path.join(self.locks, f"slot{slot}")
        env["HOLDER_READY"] = ready
        env["HOLDER_RELEASE"] = release
        proc = subprocess.Popen(["bash", self.holder], env=env)
        self._procs.append(proc)
        self.assertTrue(self.wait_until(lambda: os.path.exists(ready)),
                        "holder never became ready")
        start = proc_start_ticks(proc.pid)
        self.write_record(slot, proc.pid, start=start, crate=crate)
        return proc, ready, release, int(start)

    def hold_slot(self, slot: int) -> int:
        """Hold a slot's flock from this process; return the fd."""
        os.makedirs(self.locks, mode=0o700, exist_ok=True)
        path = os.path.join(self.locks, f"slot{slot}")
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self._fds.append(fd)
        return fd

    def unhold_slot(self, fd: int) -> None:
        self._fds.remove(fd)
        os.close(fd)

    def write_record(self, slot: int, pid, start=None, boot=None, crate="crate",
                     raw=None, extra_lines=()) -> str:
        path = os.path.join(self.locks, f"slot{slot}.holder")
        os.makedirs(self.locks, mode=0o700, exist_ok=True)
        if raw is not None:
            text = raw
        else:
            boot = boot if boot is not None else read_boot_id()
            if start is None:
                start = proc_start_ticks(int(pid))
            text = (f"pid={pid}\nboot_id={boot}\nstart_ticks={start}\n"
                    f"slot={slot}\ncrate={crate}\n")
            for line in extra_lines:
                text += line + "\n"
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def slot_path(self, slot: int) -> str:
        return os.path.join(self.locks, f"slot{slot}")

    # --- (a) probes pass straight through ----------------------------------

    def test_probe_passes_through_unchanged(self) -> None:
        for args in (["-vV"], ["--print", "cfg"], ["--print=sysroot"]):
            run = self.run_ok(*args, slots=2)
            records = run.argv_records()
            self.assertEqual(len(records), 1, records)
            self.assertEqual(records[0][1:], list(args), records)
        # A probe must not even create the lock directory.
        self.assertFalse(os.path.exists(self.locks))

    # --- (b) argv and exit status survive exec -----------------------------

    def test_argv_and_exit_status_are_preserved(self) -> None:
        args = ["--crate-name", "mycrate", "--emit", "link", "-o", "out.rlib"]
        run = self.spawn(*args, slots=2, extra={"FAKE_EXIT": "7"})
        self.assertEqual(run.wait(), 7, run.stderr())
        self.assertEqual(run.popen.returncode, 7)
        records = run.argv_records()
        self.assertEqual(len(records), 1, records)
        self.assertEqual(records[0][1:], args, records)
        # `exec` keeps the pid, so the published record names the running rustc.
        rustc_pid = records[0][0]
        with open(self.slot_path(0) + ".holder", encoding="utf-8") as handle:
            record = dict(line.split("=", 1) for line in handle.read().splitlines())
        self.assertEqual(record["pid"], rustc_pid)
        self.assertEqual(record["crate"], "mycrate")
        self.assertEqual(record["slot"], "0")
        self.assertEqual(record["boot_id"], read_boot_id())

    # --- (c) the slot inode survives waiters and publications --------------

    def test_slot_inode_survives_waiters_and_publications(self) -> None:
        self.run_ok("--crate-name", "seed", slots=1)          # create slot0
        inode_before = os.stat(self.slot_path(0))
        fd = self.hold_slot(0)
        waiters = [self.spawn("--crate-name", f"w{i}", slots=1) for i in range(3)]
        # All three wait on the held slot; the inode must not change.
        self.assertTrue(self.wait_until(lambda: all(w.alive() for w in waiters)))
        during = os.stat(self.slot_path(0))
        self.assertEqual((during.st_dev, during.st_ino),
                         (inode_before.st_dev, inode_before.st_ino))
        for w in waiters:
            self.assertEqual(w.argv_records(), [])
        self.unhold_slot(fd)
        for w in waiters:
            self.assertEqual(w.wait(), 0, w.stderr())
        after = os.stat(self.slot_path(0))
        self.assertEqual((after.st_dev, after.st_ino),
                         (inode_before.st_dev, inode_before.st_ino))
        self.assertTrue(os.path.exists(self.slot_path(0) + ".holder"))
        total = sum(len(w.argv_records()) for w in waiters)
        self.assertEqual(total, 3)

    # --- (d) never more than P1_RUSTC_SLOTS rustcs at once -----------------

    def test_two_slots_bound_five_concurrent_invocations(self) -> None:
        self.block_fake()
        run_dir = tempfile.mkdtemp(prefix="concurrent-", dir=self.dir)
        runs = [self.spawn("--crate-name", f"c{i}", slots=2, run_dir=run_dir)
                for i in range(5)]
        peak = 0
        stable = 0
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            peak = max(peak, runs[0].active())
            if runs[0].active() == 2 and all(r.alive() for r in runs):
                stable += 1
                if stable >= 5:
                    break
            else:
                stable = 0
            time.sleep(0.02)
        self.assertGreaterEqual(stable, 5, "never observed two stable holders")
        self.allow_fake()
        for r in runs:
            self.assertEqual(r.wait(timeout=60), 0, r.stderr())
        # Five fake rustcs ran, each exactly once, and at most two ever at once.
        self.assertEqual(len([n for n in os.listdir(run_dir) if n.startswith("argv.")]), 5)
        self.assertLessEqual(peak, 2)

    # --- (e) a dead holder's record is stale; the slot is taken when free --

    def test_dead_pid_record_is_stale_and_replaced(self) -> None:
        proc = subprocess.Popen(["/bin/true"])
        self._procs.append(proc)
        proc.wait()
        self.write_record(0, proc.pid, start=1, crate="ghost")
        fd = self.hold_slot(0)
        run = self.spawn("--crate-name", "real", slots=1)
        needle = f"rustc-serial: slot 0 holder record stale (pid {proc.pid} gone); waiting on the lock"
        self.assertTrue(self.wait_until(lambda: needle in run.stderr()),
                        run.stderr())
        self.assertEqual(run.argv_records(), [])
        self.assertEqual(run.stderr().count(needle), 1)
        self.unhold_slot(fd)
        self.assertEqual(run.wait(timeout=30), 0, run.stderr())
        records = run.argv_records()
        self.assertEqual(len(records), 1)
        with open(self.slot_path(0) + ".holder", encoding="utf-8") as handle:
            record = dict(line.split("=", 1) for line in handle.read().splitlines())
        self.assertEqual(record["pid"], records[0][0])
        self.assertEqual(record["crate"], "real")

    # --- (f) pid reuse: alive pid, wrong start time / boot id --------------

    def test_pid_reuse_record_is_stale(self) -> None:
        sleeper = subprocess.Popen(["/bin/sleep", "300"])
        self._procs.append(sleeper)
        real_start = proc_start_ticks(sleeper.pid)
        cases = [
            # alive pid, wrong start time
            f"pid={sleeper.pid}\nboot_id={read_boot_id()}\n"
            f"start_ticks={int(real_start) + 1}\nslot=0\ncrate=reused\n",
            # correct start time, wrong boot id
            f"pid={sleeper.pid}\nboot_id=00000000-0000-0000-0000-000000000000\n"
            f"start_ticks={real_start}\nslot=0\ncrate=reused\n",
        ]
        for raw in cases:
            self.write_record(0, 0, raw=raw)
            fd = self.hold_slot(0)
            run = self.spawn("--crate-name", "real", slots=1)
            needle = (f"rustc-serial: slot 0 holder record stale "
                      f"(pid {sleeper.pid} gone); waiting on the lock")
            self.assertTrue(self.wait_until(lambda: needle in run.stderr()),
                            run.stderr())
            self.assertEqual(run.stderr().count(needle), 1)
            self.assertEqual(run.argv_records(), [])
            self.unhold_slot(fd)
            self.assertEqual(run.wait(timeout=30), 0, run.stderr())
            self.assertEqual(len(run.argv_records()), 1)

    # --- (g) missing / partial / malformed records are unknown, occupied ---

    def test_unreadable_records_are_unknown_and_stay_occupied(self) -> None:
        partial = f"pid={os.getpid()}\n"                     # no boot/start
        malformed = "not a record at all\n"
        bad_pid = "pid=abc\nboot_id=x\nstart_ticks=1\nslot=0\ncrate=c\n"
        cases = [None, partial, malformed, bad_pid]
        for raw in cases:
            fd = self.hold_slot(0)
            holder = self.slot_path(0) + ".holder"
            if raw is None:
                with contextlib.suppress(FileNotFoundError):
                    os.unlink(holder)
            else:
                self.write_record(0, 0, raw=raw)
            run = self.spawn("--crate-name", "real", slots=1)
            # Bounded observation window: the waiter must stay admitted-out.
            self.assert_no_admission(run, 1.0)
            self.assertNotIn("stale", run.stderr())
            self.assertNotIn("stopped", run.stderr())
            self.unhold_slot(fd)
            self.assertEqual(run.wait(timeout=30), 0, run.stderr())
            self.assertEqual(len(run.argv_records()), 1)

    # --- (h) a child inheriting the lock fd keeps the slot occupied --------

    def test_inherited_descriptor_keeps_slot_occupied(self) -> None:
        os.makedirs(self.locks, mode=0o700, exist_ok=True)
        helper = subprocess.Popen([sys.executable, "-c", INHERIT_HELPER,
                                   self.slot_path(0)],
                                  stdout=subprocess.PIPE, text=True)
        dead_pid = helper.pid
        grandchild = int(helper.communicate()[0])
        helper.wait()
        self._orphans.append(grandchild)
        self.write_record(0, dead_pid, start=1, crate="inherited")
        run = self.spawn("--crate-name", "real", slots=1)
        needle = (f"rustc-serial: slot 0 holder record stale "
                  f"(pid {dead_pid} gone); waiting on the lock")
        self.assertTrue(self.wait_until(lambda: needle in run.stderr()),
                        run.stderr())
        self.assert_no_admission(run, 1.0)
        self.assertEqual(run.argv_records(), [],
                         "a stale record must not grant the slot")
        with contextlib.suppress(ProcessLookupError):
            os.kill(grandchild, signal.SIGKILL)
        self.assertTrue(self.wait_until(lambda: not os.path.exists(f"/proc/{grandchild}")))
        self.assertEqual(run.wait(timeout=30), 0, run.stderr())
        self.assertEqual(len(run.argv_records()), 1)

    # --- (i) one stopped holder is named once; the other slot is taken -----

    def test_one_stopped_holder_is_named_once(self) -> None:
        holder, _ready, _release, _start = self.start_holder(0, crate="stopped-crate")
        os.kill(holder.pid, signal.SIGSTOP)
        self.assertTrue(self.wait_until(lambda: proc_state(holder.pid) in ("T", "t")),
                        "holder did not stop")
        other = self.hold_slot(1)
        run = self.spawn("--crate-name", "real", slots=2)
        needle = (f"rustc-serial: slot 0 held by stopped pid {holder.pid} "
                  f"(crate stopped-crate); waiting for another slot")
        self.assertTrue(self.wait_until(lambda: needle in run.stderr()),
                        run.stderr())
        self.assertEqual(run.stderr().count(needle), 1)
        self.assert_no_admission(run, 0.5)
        self.unhold_slot(other)
        self.assertEqual(run.wait(timeout=30), 0, run.stderr())
        self.assertEqual(len(run.argv_records()), 1)
        # The stopped holder's slot was left alone.
        with open(self.slot_path(0) + ".holder", encoding="utf-8") as handle:
            self.assertIn(f"pid={holder.pid}\n", handle.read())
        os.kill(holder.pid, signal.SIGCONT)

    # --- (j) both stopped: wait, report, then proceed after resume ---------

    def test_both_stopped_holders_report_then_proceed(self) -> None:
        holders = []
        for slot in (0, 1):
            holder, _ready, _release, _start = self.start_holder(slot, crate=f"crate{slot}")
            os.kill(holder.pid, signal.SIGSTOP)
            self.assertTrue(self.wait_until(lambda p=holder: proc_state(p.pid) in ("T", "t")))
            holders.append(holder)
        run = self.spawn("--crate-name", "real", slots=2,
                         extra={"P1_RUSTC_SERIAL_REPORT_AFTER_S": "1"})
        for slot, holder in enumerate(holders):
            needle = (f"rustc-serial: slot {slot} held by stopped pid {holder.pid} "
                      f"(crate crate{slot}); waiting for another slot")
            self.assertTrue(self.wait_until(lambda n=needle: n in run.stderr()),
                            run.stderr())
        # Both stopped: no admission, just a one-off per-slot report. Wait for
        # both report lines: they are written in the same scan, but polling can
        # observe the first before the second has been flushed.
        line0 = f"slot 0 holder pid {holders[0].pid} state stopped crate crate0"
        line1 = f"slot 1 holder pid {holders[1].pid} state stopped crate crate1"
        self.assertTrue(self.wait_until(
            lambda: line0 in run.stderr() and line1 in run.stderr(), timeout=15),
            run.stderr())
        self.assertEqual(run.stderr().count("state stopped"), 2, run.stderr())
        self.assertEqual(run.argv_records(), [], "must not admit a third compilation")
        # Resume and let the holders exit: the waiter proceeds.
        release = os.path.join(self.dir, "holder-release-0")
        release1 = os.path.join(self.dir, "holder-release-1")
        for holder in holders:
            os.kill(holder.pid, signal.SIGCONT)
        open(release, "w").close()
        open(release1, "w").close()
        self.assertEqual(run.wait(timeout=30), 0, run.stderr())
        self.assertEqual(len(run.argv_records()), 1)
        for holder in holders:
            self.assertEqual(holder.wait(timeout=10), 0)

    # --- (k) misconfiguration is refused -----------------------------------

    def test_invalid_slot_counts_are_refused(self) -> None:
        for value in ("0", "-1", "abc", "1.5", "2x", " 3"):
            run = self.spawn("--crate-name", "x", slots=value)
            self.assertEqual(run.wait(timeout=10), 2)
            self.assertIn("P1_RUSTC_SLOTS", run.stderr())
            self.assertEqual(run.argv_records(), [])

    def test_fresh_lock_dir_is_owner_only(self) -> None:
        self.run_ok("--crate-name", "x", slots=2)
        self.assertEqual(stat.S_IMODE(os.stat(self.locks).st_mode), 0o700)

    def test_pre_existing_loose_lock_dir_is_tightened(self) -> None:
        os.makedirs(self.locks, mode=0o777)
        os.chmod(self.locks, 0o777)
        self.run_ok("--crate-name", "x", slots=2)
        self.assertEqual(stat.S_IMODE(os.stat(self.locks).st_mode), 0o700)

    def test_symlinked_lock_dir_is_refused(self) -> None:
        real = os.path.join(self.dir, "real-locks")
        os.makedirs(real)
        link = os.path.join(self.dir, "link-locks")
        os.symlink(real, link)
        run = self.spawn("--crate-name", "x", slots=2, lock_dir=link)
        self.assertEqual(run.wait(timeout=10), 2)
        self.assertIn("symlink", run.stderr())
        self.assertEqual(run.argv_records(), [])
        self.assertEqual(os.listdir(real), [], "must not use a symlinked lock dir")

    def test_symlinked_slot_is_refused(self) -> None:
        os.makedirs(self.locks, mode=0o700)
        target = os.path.join(self.dir, "elsewhere")
        with open(target, "w", encoding="utf-8"):
            pass
        os.symlink(target, os.path.join(self.locks, "slot0"))
        run = self.spawn("--crate-name", "x", slots=1)
        self.assertEqual(run.wait(timeout=10), 2)
        self.assertIn("symlink", run.stderr())
        self.assertEqual(run.argv_records(), [])


if __name__ == "__main__":
    unittest.main()
