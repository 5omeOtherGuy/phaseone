#!/usr/bin/env python3
"""Unit tests for scripts/target-template.sh --absorb — stdlib unittest, no Cargo.

    python3 scripts/test_target_template.py

Each test builds a synthetic template and target from fingerprint directories and outputs named
the way Cargo names them, under a temporary HOME; nothing below the real HOME is touched.
"""
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "target-template.sh")

OLD_RUSTC = 111
NEW_RUSTC = 222


def unit(root: str, name: str, unit_hash: str, rustc: int, mtime: int, content: str = "built") -> None:
    """One library unit: its fingerprint, its outputs and a build-script out/ directory."""
    lib = name.replace("-", "_")
    files = {
        f"debug/.fingerprint/{name}-{unit_hash}/lib-{lib}.json": f'{{"rustc":{rustc},"features":"[]"}}',
        f"debug/.fingerprint/{name}-{unit_hash}/lib-{lib}": "fingerprint",
        f"debug/deps/lib{lib}-{unit_hash}.rlib": content,
        f"debug/deps/{lib}-{unit_hash}.d": "dep-info",
        f"debug/build/{name}-{unit_hash}/out/generated.rs": content,
    }
    for rel, text in files.items():
        path = os.path.join(root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)
        os.utime(path, (mtime, mtime))
    os.makedirs(os.path.join(root, f"debug/build/{name}-{unit_hash}/out/empty"), exist_ok=True)


def files(root: str) -> set[str]:
    return {
        os.path.relpath(os.path.join(d, n), root)
        for d, dirs, names in os.walk(root)
        for n in names + [x for x in dirs if not os.listdir(os.path.join(d, x))]
    }


class AbsorbTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.home = self.tmp.name
        self.template = os.path.join(self.home, ".cache/cargo-target/p1-template")
        self.target = os.path.join(self.home, "target")
        unit(self.template, "serde", "1111111111111111", NEW_RUSTC, 2_000, "template's serde")
        unit(self.template, "oldcrate", "5555555555555555", OLD_RUSTC, 1_000)
        unit(self.target, "serde", "1111111111111111", NEW_RUSTC, 3_000, "target's serde")
        unit(self.target, "anyhow", "2222222222222222", NEW_RUSTC, 3_000)
        unit(self.target, "p1-core", "4444444444444444", NEW_RUSTC, 3_000)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def absorb(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", SCRIPT, "--absorb", self.target],
            env={**os.environ, "HOME": self.home},
            capture_output=True,
            text=True,
            check=False,
        )

    def test_adds_new_dependency_units_whole_and_unshared(self) -> None:
        result = self.absorb()
        self.assertEqual(result.returncode, 0, result.stderr)
        anyhow = {f for f in files(self.target) if "2222222222222222" in f}
        self.assertEqual(len(anyhow), 6)
        self.assertLessEqual(anyhow, files(self.template))
        rlib = os.path.join(self.template, "debug/deps/libanyhow-2222222222222222.rlib")
        self.assertEqual(os.stat(rlib).st_nlink, 1)
        self.assertEqual(os.stat(rlib).st_mtime, 3_000)

    def test_never_takes_p1_units_or_overwrites_a_held_unit(self) -> None:
        result = self.absorb()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse([f for f in files(self.template) if "4444444444444444" in f])
        with open(os.path.join(self.template, "debug/deps/libserde-1111111111111111.rlib")) as f:
            self.assertEqual(f.read(), "template's serde")

    def test_drops_the_units_of_an_older_compiler(self) -> None:
        result = self.absorb()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse([f for f in files(self.template) if "5555555555555555" in f])
        self.assertTrue([f for f in files(self.template) if "1111111111111111" in f])

    def test_a_target_with_nothing_new_changes_nothing(self) -> None:
        # More paths than a pipe holds: a filter that stops reading them kills the listing.
        for n in range(600):
            for root in (self.template, self.target):
                unit(root, f"crate{n}", f"{n:016x}", NEW_RUSTC, 2_000)
        self.assertEqual(self.absorb().returncode, 0)
        before = files(self.template)
        result = self.absorb()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(files(self.template), before)

    def test_an_older_target_cannot_drop_the_newer_compiler(self) -> None:
        unit(self.target, "stale", "6666666666666666", OLD_RUSTC, 500)
        result = self.absorb()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse([f for f in files(self.template) if "6666666666666666" in f])
        self.assertTrue([f for f in files(self.template) if "2222222222222222" in f])


if __name__ == "__main__":
    unittest.main()
