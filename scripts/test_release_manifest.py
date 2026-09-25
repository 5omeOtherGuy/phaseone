#!/usr/bin/env python3
"""Unit tests for scripts/release-manifest.py — stdlib unittest, no Cargo, no network.

    python3 scripts/test_release_manifest.py -q

Every fixture lives in a temporary directory: a synthetic repository (pins, WIT,
schemas), a staged archive modules directory, a native binary and stub `rustc`/`cargo`
executables that answer `-V` on PATH. No test reads or writes state below the real HOME,
and no test depends on the machine's toolchain.
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "release-manifest.py")

COMMIT = "0123456789abcdef0123456789abcdef01234567"
TAG = "main-89abcdef0123"
RUSTC_VERSION = "rustc 1.82.0 (f6e511eec 2024-10-15)"
CARGO_VERSION = "cargo 1.82.0 (8f40164 2024-09-19)"

TOP_LEVEL_KEYS = {
    "format",
    "commit",
    "tag",
    "native",
    "toolchain",
    "runtime",
    "wit",
    "schemas",
    "packages",
    "components",
    "environment_locks",
}
TOOLCHAIN_KEYS = {"rustc", "cargo", "wasm_target", "wasm_tools", "wit_bindgen"}
RUNTIME_KEYS = {"wasmtime", "wasmtime_features"}

FULL_PINS = """# Module toolchain pins; parsed line by line, never sourced.
WASM_TARGET=wasm32-wasip2
WASMTIME=27.0.0
WASMTIME_FEATURES=component-model
WIT_BINDGEN=0.34.0
WASM_TOOLS=1.220.0
"""

RUSTC_STUB = f"#!/bin/sh\nprintf '%s\\n' '{RUSTC_VERSION}'\n"
CARGO_STUB = f"#!/bin/sh\nprintf '%s\\n' '{CARGO_VERSION}'\n"


def write_file(path: str, data: bytes, mode: int = 0o644) -> None:
    """Write one fixture file, creating its parent directories."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as handle:
        handle.write(data)
    os.chmod(path, mode)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class ReleaseManifestTest(unittest.TestCase):
    """Exercise the generator through its command line, like the release workflow does."""

    def setUp(self) -> None:
        self.tmp = tempfile.mkdtemp(prefix="release-manifest-test-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

        self.root = os.path.join(self.tmp, "repo")
        self.modules = os.path.join(self.tmp, "share", "modules")
        self.bin = os.path.join(self.tmp, "bin")
        self.bin_cargo_only = os.path.join(self.tmp, "bin-cargo-only")
        self.native = os.path.join(self.tmp, "dist", "p1-linux-x86_64")
        self.native_bytes = b"native p1 binary fixture\n"

        os.makedirs(os.path.join(self.root, "modules"))
        os.makedirs(os.path.join(self.root, "crates", "p1-module-protocol", "schema"))
        os.makedirs(os.path.join(self.modules, "packages"))
        os.mkdir(self.bin)
        os.mkdir(self.bin_cargo_only)
        write_file(os.path.join(self.bin, "rustc"), RUSTC_STUB.encode(), 0o755)
        write_file(os.path.join(self.bin, "cargo"), CARGO_STUB.encode(), 0o755)
        write_file(os.path.join(self.bin_cargo_only, "cargo"), CARGO_STUB.encode(), 0o755)
        write_file(self.native, self.native_bytes, 0o755)

        self.write_pins(FULL_PINS)
        self.schema_bytes = {
            "usage.json": b'{"title": "usage"}\n',
            "stream-event.json": b'{"title": "stream event"}\n',
        }
        for name, data in self.schema_bytes.items():
            write_file(
                os.path.join(
                    self.root, "crates", "p1-module-protocol", "schema", name
                ),
                data,
            )

    # ---- fixtures ----------------------------------------------------------------

    def write_pins(self, text: str) -> None:
        write_file(os.path.join(self.root, "modules", "toolchain.pins"), text.encode())

    def write_wit(self, name: str, data: bytes) -> None:
        write_file(os.path.join(self.root, "modules", "wit", name), data)

    def write_package(self, rel: str, data: bytes) -> str:
        path = os.path.join(self.modules, "packages", rel)
        write_file(path, data)
        return path

    def manifest_path(self, modules_dir: str | None = None) -> str:
        return os.path.join(modules_dir or self.modules, "manifest.json")

    def read_manifest_bytes(self, modules_dir: str | None = None) -> bytes:
        with open(self.manifest_path(modules_dir), "rb") as handle:
            return handle.read()

    def read_manifest(self, modules_dir: str | None = None) -> dict:
        return json.loads(self.read_manifest_bytes(modules_dir).decode("utf-8"))

    # ---- invocation --------------------------------------------------------------

    def environment(self, path: str | None = None) -> dict[str, str]:
        env = os.environ.copy()
        env["PATH"] = path or self.bin
        return env

    def generate(
        self,
        *,
        root: str | None = None,
        commit: str = COMMIT,
        tag: str | None = TAG,
        native: str | None = None,
        modules_dir: str | None = None,
        path: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        argv = [
            sys.executable,
            SCRIPT,
            "--root",
            root if root is not None else self.root,
            "--commit",
            commit,
            "--native",
            native if native is not None else self.native,
            "--modules-dir",
            modules_dir if modules_dir is not None else self.modules,
        ]
        if tag is not None:
            argv += ["--tag", tag]
        return subprocess.run(
            argv,
            cwd=self.tmp,
            env=self.environment(path),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=60,
            check=False,
        )

    def generate_ok(self, **kwargs) -> dict:
        result = self.generate(**kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)
        return self.read_manifest(kwargs.get("modules_dir"))

    def assert_refused(self, result: subprocess.CompletedProcess[str], message: str) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("release-manifest:", result.stderr)
        self.assertIn(message, result.stderr)
        self.assertFalse(
            os.path.exists(self.manifest_path()),
            "a refused run must not leave a manifest behind",
        )

    # ---- format ------------------------------------------------------------------

    def test_exact_keys_and_types(self) -> None:
        manifest = self.generate_ok()

        self.assertEqual(set(manifest), TOP_LEVEL_KEYS)
        self.assertEqual(manifest["format"], "p1-release-manifest/1")
        self.assertEqual(manifest["commit"], COMMIT)
        self.assertEqual(manifest["tag"], TAG)

        self.assertEqual(set(manifest["native"]), {"asset", "sha256"})
        self.assertEqual(manifest["native"]["asset"], "p1-linux-x86_64")
        self.assertRegex(manifest["native"]["sha256"], r"\A[0-9a-f]{64}\Z")

        self.assertEqual(set(manifest["toolchain"]), TOOLCHAIN_KEYS)
        self.assertEqual(manifest["toolchain"]["rustc"], RUSTC_VERSION)
        self.assertEqual(manifest["toolchain"]["cargo"], CARGO_VERSION)
        self.assertEqual(manifest["toolchain"]["wasm_target"], "wasm32-wasip2")
        self.assertEqual(manifest["toolchain"]["wasm_tools"], "1.220.0")
        self.assertEqual(manifest["toolchain"]["wit_bindgen"], "0.34.0")

        self.assertEqual(set(manifest["runtime"]), RUNTIME_KEYS)
        self.assertEqual(manifest["runtime"]["wasmtime"], "27.0.0")
        self.assertEqual(manifest["runtime"]["wasmtime_features"], "component-model")

        for section in ("wit", "schemas"):
            self.assertIsInstance(manifest[section], list)
            for entry in manifest[section]:
                self.assertEqual(set(entry), {"path", "sha256"})
                self.assertIsInstance(entry["path"], str)
                self.assertRegex(entry["sha256"], r"\A[0-9a-f]{64}\Z")
        for entry in manifest["packages"]:
            self.assertEqual(set(entry), {"path", "sha256", "size"})
            self.assertIsInstance(entry["size"], int)
            self.assertRegex(entry["sha256"], r"\A[0-9a-f]{64}\Z")

        # The freeze tag wasm-boundary-v1 fixes the entry shape of these two; before it
        # they are present and empty.
        self.assertEqual(manifest["components"], [])
        self.assertEqual(manifest["environment_locks"], [])

    def test_output_is_sorted_keys_two_space_indent_and_newline(self) -> None:
        self.write_package("read.wasm", b"read module\n")
        manifest = self.generate_ok()
        raw = self.read_manifest_bytes()

        self.assertTrue(raw.endswith(b"\n"))
        self.assertFalse(raw.endswith(b"\n\n"))
        self.assertTrue(raw.startswith(b'{\n  "commit": '))
        expected = (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode()
        self.assertEqual(raw, expected)

    def test_tag_may_be_omitted_for_a_candidate_build(self) -> None:
        manifest = self.generate_ok(tag=None)
        self.assertIsNone(manifest["tag"])
        self.assertEqual(manifest["format"], "p1-release-manifest/1")

    # ---- digests, sizes, sorting, determinism ------------------------------------

    def test_digests_and_sizes_match_the_staged_files(self) -> None:
        native_bytes = b"another native fixture\n"
        write_file(self.native, native_bytes, 0o755)
        read_bytes = b"read module bytes\n"
        nested_bytes = b"nested echo module bytes\n"
        self.write_package("read.wasm", read_bytes)
        self.write_package("tool/echo.wasm", nested_bytes)
        wit_bytes = b'(component)\n'
        self.write_wit("p1.wit", wit_bytes)

        manifest = self.generate_ok()

        self.assertEqual(manifest["native"]["sha256"], sha256(native_bytes))
        packages = {entry["path"]: entry for entry in manifest["packages"]}
        self.assertEqual(
            set(packages), {"packages/read.wasm", "packages/tool/echo.wasm"}
        )
        self.assertEqual(packages["packages/read.wasm"]["sha256"], sha256(read_bytes))
        self.assertEqual(packages["packages/read.wasm"]["size"], len(read_bytes))
        self.assertEqual(packages["packages/tool/echo.wasm"]["sha256"], sha256(nested_bytes))
        self.assertEqual(packages["packages/tool/echo.wasm"]["size"], len(nested_bytes))

        self.assertEqual(
            manifest["wit"], [{"path": "modules/wit/p1.wit", "sha256": sha256(wit_bytes)}]
        )
        schemas = {entry["path"]: entry["sha256"] for entry in manifest["schemas"]}
        self.assertEqual(
            schemas,
            {
                "crates/p1-module-protocol/schema/stream-event.json": sha256(
                    self.schema_bytes["stream-event.json"]
                ),
                "crates/p1-module-protocol/schema/usage.json": sha256(
                    self.schema_bytes["usage.json"]
                ),
            },
        )

    def test_schema_scan_stays_at_the_schema_directory_top_level(self) -> None:
        # The format fixes the schema set as schema/*.json (one level) and only the WIT
        # set as **/*.wit, so a nested schema is not part of the manifest S7.2 verifies.
        write_file(
            os.path.join(
                self.root,
                "crates",
                "p1-module-protocol",
                "schema",
                "nested",
                "extra.json",
            ),
            b'{"title": "nested"}\n',
        )
        self.write_wit("nested/p1.wit", b"(component)\n")

        manifest = self.generate_ok()

        self.assertEqual(
            [entry["path"] for entry in manifest["schemas"]],
            [
                "crates/p1-module-protocol/schema/stream-event.json",
                "crates/p1-module-protocol/schema/usage.json",
            ],
        )
        self.assertEqual(
            [entry["path"] for entry in manifest["wit"]],
            ["modules/wit/nested/p1.wit"],
        )

    def test_every_array_is_sorted_by_path(self) -> None:
        # Creation order is deliberately not sorted, so a pass means the generator sorts.
        self.write_package("zeta.wasm", b"z\n")
        self.write_package("alpha.wasm", b"a\n")
        self.write_package("nested/beta.wasm", b"b\n")
        self.write_wit("zz.wit", b"z\n")
        self.write_wit("aa.wit", b"a\n")
        write_file(
            os.path.join(self.root, "crates", "p1-module-protocol", "schema", "zz.json"),
            b"z\n",
        )
        write_file(
            os.path.join(self.root, "crates", "p1-module-protocol", "schema", "aa.json"),
            b"a\n",
        )

        manifest = self.generate_ok()

        for section in ("wit", "schemas", "packages"):
            paths = [entry["path"] for entry in manifest[section]]
            self.assertEqual(paths, sorted(paths), section)
            self.assertEqual(len(paths), len(set(paths)), section)
        self.assertEqual(
            [entry["path"] for entry in manifest["packages"]],
            ["packages/alpha.wasm", "packages/nested/beta.wasm", "packages/zeta.wasm"],
        )
        self.assertEqual(
            [entry["path"] for entry in manifest["wit"]],
            ["modules/wit/aa.wit", "modules/wit/zz.wit"],
        )

    def test_manifest_names_only_posix_relative_paths(self) -> None:
        self.write_package("tool/echo.wasm", b"e\n")
        self.write_wit("p1.wit", b"(component)\n")

        manifest = self.generate_ok()

        paths = [
            entry["path"]
            for section in ("wit", "schemas", "packages")
            for entry in manifest[section]
        ]
        self.assertTrue(paths)
        for path in paths:
            self.assertFalse(path.startswith("/"), path)
            self.assertNotIn("\\", path)
            for part in path.split("/"):
                self.assertNotIn(part, ("", ".", ".."), path)
        self.assertNotIn("modules/toolchain.pins", paths)

    def test_equal_inputs_give_byte_identical_output(self) -> None:
        self.write_package("read.wasm", b"read module\n")
        self.write_wit("p1.wit", b"(component)\n")

        first = self.generate()
        self.assertEqual(first.returncode, 0, first.stderr)
        first_bytes = self.read_manifest_bytes()
        second = self.generate()
        self.assertEqual(second.returncode, 0, second.stderr)

        # A second staged directory with equal content, at a different absolute path,
        # must give the same bytes: nothing absolute leaks into the manifest.
        other_modules = os.path.join(self.tmp, "other-share", "modules")
        os.makedirs(os.path.join(other_modules, "packages"))
        write_file(os.path.join(other_modules, "packages", "read.wasm"), b"read module\n")
        third = self.generate(modules_dir=other_modules)
        self.assertEqual(third.returncode, 0, third.stderr)

        self.assertEqual(self.read_manifest_bytes(), first_bytes)
        self.assertEqual(self.read_manifest_bytes(other_modules), first_bytes)

    # ---- scaffold ----------------------------------------------------------------

    def test_scaffold_has_empty_arrays_and_null_unpinned_fields(self) -> None:
        # Before the freeze tag: an empty package set, no WIT directory, and pins that
        # name nothing this slice knows are null rather than invented.
        self.write_pins("# nothing is pinned yet\n\nWASM_TARGET=wasm32-wasip2\n")
        os.rmdir(os.path.join(self.modules, "packages"))

        manifest = self.generate_ok()

        self.assertEqual(manifest["packages"], [])
        self.assertEqual(manifest["wit"], [])
        self.assertEqual(manifest["components"], [])
        self.assertEqual(manifest["environment_locks"], [])
        self.assertEqual(manifest["toolchain"]["wasm_target"], "wasm32-wasip2")
        self.assertEqual(manifest["toolchain"]["rustc"], RUSTC_VERSION)
        for missing in ("wasm_tools", "wit_bindgen"):
            self.assertIsNone(manifest["toolchain"][missing], missing)
        for missing in ("wasmtime", "wasmtime_features"):
            self.assertIsNone(manifest["runtime"][missing], missing)

    def test_staged_modules_directory_holds_only_the_manifest_and_packages(self) -> None:
        self.write_package("read.wasm", b"read module\n")

        self.generate_ok()

        self.assertEqual(sorted(os.listdir(self.modules)), ["manifest.json", "packages"])
        raw = self.read_manifest_bytes().decode("utf-8")
        self.assertNotIn("toolchain.pins", raw)
        self.assertNotIn("modules/wit", raw)
        self.assertNotIn("Cargo.toml", raw)

    # ---- pins --------------------------------------------------------------------

    def test_pins_value_with_a_command_is_never_executed(self) -> None:
        marker = os.path.join(self.tmp, "pins-command-ran")
        # The value has no whitespace, so it is a well-formed KEY=value line and is kept
        # verbatim; sourcing the file would run touch and create the marker.
        self.write_pins(
            "# a pin whose value looks like a command is data, not code\n"
            f"WASM_TOOLS=$(touch${{IFS}}{marker})\n"
        )

        manifest = self.generate_ok()

        self.assertEqual(
            manifest["toolchain"]["wasm_tools"], f"$(touch${{IFS}}{marker})"
        )
        self.assertFalse(
            os.path.exists(marker), "the pins file was executed rather than parsed"
        )

    def test_pin_line_that_is_not_key_value_is_refused(self) -> None:
        self.write_pins("WASM_TOOLS wasm-tools 1.220.0\n")

        result = self.generate()

        self.assert_refused(result, "not a KEY=value line")
        self.assertIn("toolchain.pins:1", result.stderr)

    def test_missing_pins_file_is_refused(self) -> None:
        os.remove(os.path.join(self.root, "modules", "toolchain.pins"))

        result = self.generate()

        self.assert_refused(result, "toolchain.pins")

    # ---- toolchain ---------------------------------------------------------------

    def test_missing_rustc_gives_null_and_cargo_is_recorded(self) -> None:
        manifest = self.generate_ok(path=self.bin_cargo_only)

        self.assertIsNone(manifest["toolchain"]["rustc"])
        self.assertEqual(manifest["toolchain"]["cargo"], CARGO_VERSION)

    def test_missing_cargo_gives_null_and_rustc_is_recorded(self) -> None:
        bin_rustc_only = os.path.join(self.tmp, "bin-rustc-only")
        os.mkdir(bin_rustc_only)
        write_file(os.path.join(bin_rustc_only, "rustc"), RUSTC_STUB.encode(), 0o755)

        manifest = self.generate_ok(path=bin_rustc_only)

        self.assertEqual(manifest["toolchain"]["rustc"], RUSTC_VERSION)
        self.assertIsNone(manifest["toolchain"]["cargo"])

    # ---- refusals ----------------------------------------------------------------

    def test_symlink_under_packages_is_refused(self) -> None:
        victim = os.path.join(self.tmp, "victim.wasm")
        write_file(victim, b"outside the archive\n")
        link = os.path.join(self.modules, "packages", "linked.wasm")
        os.symlink(victim, link)

        result = self.generate()

        self.assert_refused(result, "symlink under packages/")
        self.assertEqual(os.readlink(link), victim)

    def test_symlinked_directory_under_packages_is_refused(self) -> None:
        elsewhere = os.path.join(self.tmp, "elsewhere")
        write_file(os.path.join(elsewhere, "read.wasm"), b"outside the archive\n")
        os.symlink(elsewhere, os.path.join(self.modules, "packages", "tool"))

        result = self.generate()

        self.assert_refused(result, "symlink under packages/")

    @unittest.skipUnless(hasattr(os, "mkfifo"), "mkfifo is required")
    def test_fifo_under_packages_is_refused(self) -> None:
        fifo = os.path.join(self.modules, "packages", "pipe.wasm")
        try:
            os.mkfifo(fifo)
        except OSError as exc:  # pragma: no cover - filesystem without FIFO support
            self.skipTest(f"mkfifo unavailable: {exc}")

        result = self.generate()

        self.assert_refused(result, "special file under packages/")

    def test_cwasm_blob_under_packages_is_refused(self) -> None:
        self.write_package("read.wasm", b"read module\n")
        self.write_package("nested/read.wasm.cwasm", b"compiled cache\n")

        result = self.generate()

        self.assert_refused(result, "compiled-cache blob")

    def test_bad_commit_is_refused(self) -> None:
        bad_commits = {
            "empty": "",
            "short": COMMIT[:-1],
            "long": COMMIT + "0",
            "uppercase": COMMIT.upper(),
            "non-hex": "g" * 40,
            "not a commit": "main",
        }
        for label, commit in bad_commits.items():
            with self.subTest(commit=label):
                result = self.generate(commit=commit)
                self.assert_refused(result, "40 lowercase hex")

    def test_missing_native_file_is_refused(self) -> None:
        result = self.generate(native=os.path.join(self.tmp, "dist", "absent"))

        self.assert_refused(result, "missing")

    def test_native_directory_is_refused(self) -> None:
        result = self.generate(native=os.path.dirname(self.native))

        self.assert_refused(result, "missing")

    # ---- the published archive ---------------------------------------------------

    def test_release_workflow_packs_the_staged_share_tree(self) -> None:
        # The workflow is what publishes the archive, so its commands are pinned here:
        # a share tree staged with an empty modules/packages/, then packed with -C
        # dist/share so the repository's own modules/ sources can never be an argument.
        workflow = os.path.join(
            os.path.dirname(HERE), ".github", "workflows", "release.yml"
        )
        with open(workflow, encoding="utf-8") as handle:
            text = handle.read()

        for line in (
            "mkdir -p dist/share/modules/packages",
            "cp -a environments routes profiles dist/share/",
            '--commit "${{ github.event.workflow_run.head_sha }}"',
            '--tag "${{ steps.tag.outputs.tag }}"',
            "--native dist/p1-linux-x86_64",
            "--modules-dir dist/share/modules",
            "tar -czf dist/p1-share.tar.gz -C dist/share environments routes profiles modules",
        ):
            self.assertIn(line, text, line)
        self.assertNotIn("tar -czf dist/p1-share.tar.gz environments", text)
        self.assertIn("find dist/share/modules -name '*.cwasm'", text)
        for asset in (
            "p1-linux-x86_64",
            "p1-linux-x86_64.sha256",
            "p1-share.tar.gz",
            "p1-share.tar.gz.sha256",
        ):
            self.assertIn(f"dist/{asset}", text, asset)

    @unittest.skipUnless(shutil.which("tar"), "tar is required")
    def test_share_archive_carries_the_manifest_and_packages_beside_the_payload(self) -> None:
        # Stage the share tree exactly as the release workflow does, then pack it with
        # the workflow's own tar command line.
        share = os.path.join(self.tmp, "dist", "share")
        for name in ("environments", "routes", "profiles"):
            write_file(os.path.join(share, name, "default.json"), b'{"name": "default"}\n')
        os.makedirs(os.path.join(share, "modules", "packages"))

        result = self.generate(modules_dir=os.path.join(share, "modules"))
        self.assertEqual(result.returncode, 0, result.stderr)

        archive = os.path.join(self.tmp, "dist", "p1-share.tar.gz")
        packed = subprocess.run(
            [
                "tar",
                "-czf",
                archive,
                "-C",
                share,
                "environments",
                "routes",
                "profiles",
                "modules",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=60,
            check=False,
        )
        self.assertEqual(packed.returncode, 0, packed.stderr)

        with tarfile.open(archive, "r:gz") as tar:
            members = sorted(
                (member.name, member.isdir()) for member in tar.getmembers()
            )
            names = [name for name, _ in members]
            manifest_bytes = tar.extractfile("modules/manifest.json").read()

        self.assertEqual(
            members,
            [
                ("environments", True),
                ("environments/default.json", False),
                ("modules", True),
                ("modules/manifest.json", False),
                ("modules/packages", True),
                ("profiles", True),
                ("profiles/default.json", False),
                ("routes", True),
                ("routes/default.json", False),
            ],
        )
        self.assertEqual(manifest_bytes, self.read_manifest_bytes(os.path.join(share, "modules")))
        manifest = json.loads(manifest_bytes.decode("utf-8"))
        self.assertEqual(manifest["packages"], [])
        self.assertEqual(manifest["components"], [])
        self.assertFalse([name for name in names if name.endswith(".cwasm")])
        self.assertNotIn("modules/toolchain.pins", names)
        self.assertFalse([name for name in names if name.startswith("modules/wit")])


if __name__ == "__main__":
    unittest.main()
