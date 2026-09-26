#!/usr/bin/env python3
"""Unit tests for scripts/stage-release.sh — stdlib unittest, temp dirs only, no network.

    python3 scripts/test_stage_release.py -q

A fake binary and a fixture build-outputs directory (one directory per package, as
`scripts/build-modules.sh` publishes them: the component, its sha256, its frozen manifest
and the build's other files) stand in for a real build. Every staged archive is read back
from a temporary --out; the shipped environments/, routes/ and profiles/ come from this
checkout, so the archive is checked to hold the checkout's own modules/ sources never.
Nothing reads or writes state below the real HOME.
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import tarfile
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "stage-release.sh")

# The interpreter by absolute path, so the script under test is the only moving part.
BASH = shutil.which("bash") or "/bin/bash"

COMMIT = "0123456789abcdef0123456789abcdef01234567"
TAG = "main-89abcdef0123"

ASSETS = ("p1-linux-x86_64", "p1-linux-x86_64.sha256",
          "p1-share.tar.gz", "p1-share.tar.gz.sha256")

# The four top-level roots the installer accepts, and nothing else.
ROOTS = ("environments", "modules", "profiles", "routes")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_file(path: str, data: bytes, mode: int = 0o644) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as handle:
        handle.write(data)
    os.chmod(path, mode)


class StageReleaseTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.mkdtemp(prefix="stage-release-test-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.modules = os.path.join(self.tmp, "p1-modules")
        os.makedirs(self.modules)
        self.out = os.path.join(self.tmp, "dist")
        self.binary_bytes = b"fake p1 binary bytes\n"
        self.binary = os.path.join(self.tmp, "p1")
        write_file(self.binary, self.binary_bytes, 0o755)

    # ---- fixtures ----------------------------------------------------------------

    def package(self, crate: str, name: str, data: bytes, **overrides) -> dict:
        """One build output under the fixture modules directory, with its `.wasm`."""
        manifest = {
            "name": name,
            "kind": "tool",
            "world": "p1:module/tool@1.0.0",
            "protocol": "1.0",
            "capabilities": ["control", "clock", "process"],
            "variant": "default",
            "digest": "sha256:" + sha256(data),
            "size": len(data),
        }
        manifest.update(overrides)
        out = os.path.join(self.modules, crate)
        write_file(os.path.join(out, f"{crate}.wasm"), data)
        write_file(os.path.join(out, f"{crate}.wit"), b"(component)\n")
        write_file(os.path.join(out, f"{crate}.imports"), b"p1:module/types\n")
        write_file(os.path.join(out, f"{crate}.sha256"),
                   f"{sha256(data)}  {crate}.wasm\n".encode())
        write_file(os.path.join(out, f"{crate}.manifest.json"),
                   (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode())
        return manifest

    def fixture(self) -> bytes:
        """One package, as the tag ships it, and its component bytes."""
        data = b"fixture component bytes\n"
        self.package("p1-module-fixture", "p1/fixture", data)
        return data

    # ---- invocation --------------------------------------------------------------

    def stage(self, *args: str, out: str | None = None,
              modules: str | None = None, native: str | None = None,
              commit: str = COMMIT, tag: str | None = TAG) -> subprocess.CompletedProcess:
        argv = [
            BASH, SCRIPT,
            "--native", native if native is not None else self.binary,
            "--out", out if out is not None else self.out,
            "--commit", commit,
            "--modules", modules if modules is not None else self.modules,
        ]
        if tag is not None:
            argv += ["--tag", tag]
        argv += list(args)
        return subprocess.run(argv, cwd=self.tmp, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def assert_no_out(self, done: subprocess.CompletedProcess, message: str) -> None:
        # A rejected input is exit 1 by the script's frozen contract; the usage errors, which
        # exit 2, are pinned by their own cases.
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("stage-release:", done.stderr)
        self.assertIn(message, done.stderr)
        self.assertFalse(os.path.exists(self.out),
                         f"a refused run left {self.out} behind")

    # ---- readers -----------------------------------------------------------------

    def read(self, path: str) -> bytes:
        with open(path, "rb") as handle:
            return handle.read()

    def sha_line(self, asset: str) -> str:
        """The digest the asset's .sha256 file names."""
        return self.read(os.path.join(self.out, asset + ".sha256")).decode().split()[0]

    def members(self) -> dict[str, bytes]:
        """Every regular file of the staged share archive, by member name."""
        found = {}
        with tarfile.open(os.path.join(self.out, "p1-share.tar.gz"), "r:gz") as archive:
            for member in archive.getmembers():
                if member.isfile():
                    found[member.name] = archive.extractfile(member).read()
        return found

    def manifest(self) -> dict:
        return json.loads(self.members()["modules/manifest.json"].decode("utf-8"))

    # ---- the four assets ---------------------------------------------------------

    def test_writes_exactly_the_four_assets_with_their_checksums(self) -> None:
        self.fixture()

        done = self.stage()

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(sorted(os.listdir(self.out)), sorted(ASSETS))
        binary = os.path.join(self.out, "p1-linux-x86_64")
        self.assertEqual(self.read(binary), self.binary_bytes)
        self.assertEqual(stat.S_IMODE(os.stat(binary).st_mode), 0o755)
        self.assertEqual(self.sha_line("p1-linux-x86_64"), sha256(self.binary_bytes))
        self.assertEqual(self.sha_line("p1-share.tar.gz"),
                         sha256(self.read(os.path.join(self.out, "p1-share.tar.gz"))))

    def test_the_share_archive_carries_the_shipped_roots_and_the_module_set(self) -> None:
        data = self.fixture()

        done = self.stage()

        self.assertEqual(done.returncode, 0, done.stderr)
        files = self.members()
        roots = {name.split("/", 1)[0] for name in files}
        self.assertEqual(roots, set(ROOTS))
        self.assertIn("modules/manifest.json", files)
        self.assertEqual(files["modules/packages/p1-fixture/p1-fixture.wasm"], data)
        for root in ("environments", "routes", "profiles"):
            self.assertTrue([name for name in files if name.startswith(root + "/")], root)
        # Only the component is shipped: none of the build's other files reaches the archive.
        for name in files:
            self.assertNotIn(name.rsplit("/", 1)[-1],
                             ("p1-module-fixture.wit", "p1-module-fixture.imports",
                              "p1-module-fixture.sha256", "p1-module-fixture.manifest.json"))

    def test_the_manifest_binds_every_shipped_file(self) -> None:
        data = self.fixture()

        self.assertEqual(self.stage().returncode, 0)

        manifest = self.manifest()
        self.assertEqual(manifest["format"], "p1-release-manifest/1")
        self.assertEqual(manifest["commit"], COMMIT)
        self.assertEqual(manifest["tag"], TAG)
        self.assertEqual(manifest["environment_locks"], [])
        self.assertEqual(
            manifest["packages"],
            [{"path": "packages/p1-fixture/p1-fixture.wasm",
              "sha256": sha256(data), "size": len(data)}],
        )
        self.assertEqual(
            manifest["components"],
            [{"name": "p1/fixture",
              "digest": "sha256:" + sha256(data),
              "path": "packages/p1-fixture/p1-fixture.wasm",
              "kind": "tool",
              "world": "p1:module/tool@1.0.0",
              "protocol": "1.0",
              "capabilities": ["control", "clock", "process"],
              "variant": "default"}],
        )
        staged = {name for name in self.members() if name.startswith("modules/packages/")}
        self.assertEqual(staged,
                         {"modules/" + entry["path"] for entry in manifest["packages"]})
        # The archive member's bytes are the ones the manifest names.
        for entry in manifest["packages"]:
            self.assertEqual(sha256(self.members()["modules/" + entry["path"]]),
                             entry["sha256"])

    def test_several_packages_are_all_staged(self) -> None:
        self.package("p1-module-fixture", "p1/fixture", b"fixture\n")
        self.package("p1-module-other", "p1/other", b"other\n")

        self.assertEqual(self.stage().returncode, 0)

        manifest = self.manifest()
        self.assertEqual([entry["name"] for entry in manifest["components"]],
                         ["p1/fixture", "p1/other"])
        self.assertEqual(sorted(member for member in self.members()
                                if member.startswith("modules/packages/")),
                         ["modules/packages/p1-fixture/p1-fixture.wasm",
                          "modules/packages/p1-other/p1-other.wasm"])

    # ---- refusals ----------------------------------------------------------------

    def test_a_compiled_cache_blob_is_refused_and_leaves_no_out(self) -> None:
        self.fixture()
        write_file(os.path.join(self.modules, "p1-module-fixture",
                                "p1-module-fixture.cwasm"), b"compiled cache\n")

        self.assert_no_out(self.stage(), "a compiled-cache blob is never shipped")

    def test_a_symlink_is_refused_and_leaves_no_out(self) -> None:
        self.fixture()
        wasm = os.path.join(self.modules, "p1-module-fixture", "p1-module-fixture.wasm")
        os.remove(wasm)
        os.symlink(self.binary, wasm)

        self.assert_no_out(self.stage(), "a symlink is never shipped")

    def test_a_digest_mismatch_is_refused_and_leaves_no_out(self) -> None:
        self.fixture()
        write_file(os.path.join(self.modules, "p1-module-fixture",
                                "p1-module-fixture.sha256"),
                   f"{'0' * 64}  p1-module-fixture.wasm\n".encode())

        self.assert_no_out(self.stage(), ".sha256 says")

    def test_a_build_output_that_is_not_a_package_format_file_is_refused(self) -> None:
        self.fixture()
        write_file(os.path.join(self.modules, "p1-module-fixture", "stray.bin"), b"stray\n")

        self.assert_no_out(self.stage(), "not a file the module package format ships")

    def test_a_missing_component_is_refused_and_leaves_no_out(self) -> None:
        self.fixture()
        os.remove(os.path.join(self.modules, "p1-module-fixture", "p1-module-fixture.wasm"))

        self.assert_no_out(self.stage(), "no p1-module-fixture.wasm")

    def test_a_missing_binary_is_refused_and_leaves_no_out(self) -> None:
        self.fixture()

        self.assert_no_out(
            self.stage(native=os.path.join(self.tmp, "absent")), "--native")

    def test_no_module_packages_is_refused_and_leaves_no_out(self) -> None:
        self.assert_no_out(self.stage(), "no module packages to ship")

    def test_a_bad_commit_is_refused_and_leaves_no_out(self) -> None:
        self.fixture()

        for commit in ("main", COMMIT[:-1], COMMIT.upper(), "g" * 40):
            with self.subTest(commit=commit):
                self.assert_no_out(self.stage(commit=commit), "--commit")

    def test_a_missing_required_argument_is_a_usage_error(self) -> None:
        done = subprocess.run([BASH, SCRIPT], stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn("usage: scripts/stage-release.sh", done.stderr)

    def test_an_unknown_argument_is_a_usage_error(self) -> None:
        done = self.stage("--bogus")

        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn("unknown argument", done.stderr)

    # ---- the checkout's own sources ----------------------------------------------

    def test_the_repository_modules_sources_are_never_packed(self) -> None:
        self.fixture()

        self.assertEqual(self.stage().returncode, 0)

        members = set(self.members())
        modules_members = {name for name in members if name.startswith("modules/")}
        self.assertEqual(
            modules_members,
            {"modules/manifest.json", "modules/packages/p1-fixture/p1-fixture.wasm"},
        )
        for leaked in ("modules/toolchain.pins", "modules/Cargo.toml", "modules/Cargo.lock",
                       "modules/capabilities.toml", "modules/wit"):
            self.assertNotIn(leaked, members, leaked)
        self.assertFalse([name for name in members if "p1-module-fixture/src" in name])


if __name__ == "__main__":
    unittest.main()
