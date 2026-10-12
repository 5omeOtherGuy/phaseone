#!/usr/bin/env python3
"""Unit tests for scripts/stage-release.sh — stdlib unittest, temp dirs only, no network.

    python3 scripts/test_stage_release.py -q

A fake binary and a fixture build-outputs directory (one directory per package, as
`scripts/build-modules.sh` publishes them: the component, its sha256, its frozen manifest
and the build's other files) stand in for a real build. Every staged archive is read back
from a temporary --out; the shipped environments/, routes/, profiles/ and accounts/ come from this
checkout, so the archive is checked to hold the checkout's own modules/ sources never.
Nothing reads or writes state below the real HOME.
"""
from __future__ import annotations

import glob
import hashlib
import json
import os
import shutil
import stat
import subprocess
import tarfile
import tempfile
import tomllib
import unittest
import unittest.mock

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "stage-release.sh")

# The interpreter by absolute path, so the script under test is the only moving part.
BASH = shutil.which("bash") or "/bin/bash"

COMMIT = "0123456789abcdef0123456789abcdef01234567"
TAG = "main-89abcdef0123"

ASSETS = ("p1-linux-x86_64", "p1-linux-x86_64.sha256",
          "p1-share.tar.gz", "p1-share.tar.gz.sha256")

# The five top-level roots the installer accepts, and nothing else.
ROOTS = ("accounts", "environments", "modules", "profiles", "routes")


FAKE_P1 = """#!/bin/sh
set -eu
[ "$1 $2 $3" = "modules precompile --root" ] || exit 9
for wasm in "$4"/packages/*/*.wasm; do
  printf 'compiled %s\\n' "$(basename "$wasm")" >"${wasm%.wasm}.cwasm"
done
"""


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
        # The fake p1 answers the one command staging runs, `modules precompile --root DIR`,
        # by writing a deterministic compiled copy beside each staged component.
        self.binary_bytes = FAKE_P1.encode()
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
        """One package, as the tag ships it, and its component bytes; beside it the build
        outputs of every connector's packages, which the shipped roots ship (ADR-0217)."""
        data = b"fixture component bytes\n"
        self.package("p1-module-fixture", "p1/fixture", data)
        self.connector_packages()
        return data

    def connector_packages(self) -> list[str]:
        """Fixture build outputs for every package of a connector (connectors/*/components)."""
        repo = os.path.dirname(HERE)
        crates = []
        for manifest in sorted(glob.glob(os.path.join(repo, "connectors/*/components/*/Cargo.toml"))):
            with open(manifest, "rb") as handle:
                package = tomllib.load(handle)["package"]
            metadata = package.get("metadata", {}).get("p1-module")
            if metadata is None:
                continue
            crates.append(package["name"])
            self.package(package["name"], metadata["name"], b"connector component bytes\n",
                         **{key: value for key, value in metadata.items() if key != "name"})
        return crates

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

    def test_two_independent_stages_of_one_commit_are_byte_identical(self) -> None:
        self.fixture()
        first = self.stage()
        self.assertEqual(first.returncode, 0, first.stderr)
        other = os.path.join(self.tmp, 'other-dist')
        second = self.stage(out=other)
        self.assertEqual(second.returncode, 0, second.stderr)
        for asset in ASSETS:
            with self.subTest(asset=asset):
                self.assertEqual(self.read(os.path.join(self.out, asset)),
                                 self.read(os.path.join(other, asset)))

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

    def test_packed_bytes_are_reconciled_against_manifest(self) -> None:
        self.fixture()
        stub = os.path.join(self.tmp, 'bin')
        os.mkdir(stub)
        tar_path = os.path.join(stub, 'tar')
        with open(tar_path, 'w', encoding='utf-8') as output:
            output.write('''#!/bin/sh
/usr/bin/tar "$@" || exit
python3 - "$2" <<'PY'
import io, os, sys, tarfile
path = sys.argv[1]
with tarfile.open(path, 'r:gz') as source:
    entries = [(member, source.extractfile(member).read() if member.isfile() else None)
               for member in source]
with tarfile.open(path + '.changed', 'w:gz') as dest:
    for member, data in entries:
        if member.name.endswith('.wasm'):
            data = b'X' * len(data)
        dest.addfile(member, io.BytesIO(data) if data is not None else None)
os.replace(path + '.changed', path)
PY
''')
        os.chmod(tar_path, 0o755)
        with unittest.mock.patch.dict(os.environ, {'PATH': stub + ':' + os.environ['PATH']}):
            done = self.stage()
        self.assertNotEqual(done.returncode, 0, done.stderr)
        self.assertFalse(os.path.exists(self.out))

    def test_symlink_output_does_not_delete_sentinel(self) -> None:
        self.fixture()
        sentinel = os.path.join(self.tmp, 'valued')
        os.mkdir(sentinel)
        write_file(os.path.join(sentinel, 'keep'), b'untouched')
        os.symlink(sentinel, self.out)
        done = self.stage()
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(self.read(os.path.join(sentinel, 'keep')), b'untouched')

    def test_four_public_asset_names_do_not_prove_output_ownership(self) -> None:
        self.fixture()
        os.mkdir(self.out)
        before = {}
        for asset in ASSETS:
            value = ('unrelated ' + asset).encode()
            write_file(os.path.join(self.out, asset), value)
            before[asset] = value
        done = self.stage()
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual({asset: self.read(os.path.join(self.out, asset))
                          for asset in ASSETS}, before)

    def test_unowned_output_does_not_delete_sentinel(self) -> None:
        self.fixture()
        os.mkdir(self.out)
        write_file(os.path.join(self.out, 'keep'), b'untouched')
        done = self.stage()
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(self.read(os.path.join(self.out, 'keep')), b'untouched')

    def test_failed_replacement_restores_prior_complete_stage(self) -> None:
        self.fixture()
        first = self.stage()
        self.assertEqual(first.returncode, 0, first.stderr)
        before = {asset: self.read(os.path.join(self.out, asset)) for asset in ASSETS}
        tools = os.path.join(self.tmp, 'bin')
        os.mkdir(tools)
        stub = os.path.join(tools, 'mv')
        with open(stub, 'w', encoding='utf-8') as output:
            output.write('''#!/bin/sh
case "$4" in
  */dist) case "$3" in *release-backup*) ;; *) exit 72 ;; esac ;;
esac
exec /usr/bin/mv "$@"
''')
        os.chmod(stub, 0o755)
        with unittest.mock.patch.dict(os.environ, {'PATH': tools + ':' + os.environ['PATH']}):
            failed = self.stage()
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual({asset: self.read(os.path.join(self.out, asset)) for asset in ASSETS}, before)

    def test_failed_owner_record_restores_the_prior_owned_stage(self) -> None:
        # A failure after the new assets are in place but before the new ownership record is
        # committed must not strand the old stage in a backup: the prior complete stage is
        # restored, so a later run does not refuse it as unowned.
        self.fixture()
        first = self.stage()
        self.assertEqual(first.returncode, 0, first.stderr)
        before = {asset: self.read(os.path.join(self.out, asset)) for asset in ASSETS}
        # The replacement must differ, so a newly published stage is distinguishable.
        write_file(self.binary, FAKE_P1.encode() + b"# replacement\n", 0o755)
        tools = os.path.join(self.tmp, 'bin')
        os.mkdir(tools)
        stub = os.path.join(tools, 'mv')
        with open(stub, 'w', encoding='utf-8') as output:
            output.write('''#!/bin/sh
case "$4" in
  *.p1-stage-owner) exit 71 ;;
esac
exec /usr/bin/mv "$@"
''')
        os.chmod(stub, 0o755)
        with unittest.mock.patch.dict(os.environ, {'PATH': tools + ':' + os.environ['PATH']}):
            failed = self.stage()
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual({asset: self.read(os.path.join(self.out, asset)) for asset in ASSETS}, before)
        # The restored stage still carries its matching record: a later stage is not refused.
        third = self.stage()
        self.assertEqual(third.returncode, 0, third.stderr)

    def test_directory_at_the_owner_record_path_is_refused(self) -> None:
        # A directory at the sibling ownership-record path makes `mv <record> <path>` nest
        # the record inside it, leaving the published stage unowned while the script still
        # reports success (Codex finding stage-release.sh:276).
        self.fixture()
        record = os.path.join(self.tmp, '.dist.p1-stage-owner')
        os.mkdir(record)
        write_file(os.path.join(record, 'keep'), b'untouched')

        done = self.stage()

        self.assertNotEqual(done.returncode, 0, done.stderr)
        self.assertIn('ownership record', done.stderr)
        self.assertEqual(os.listdir(record), ['keep'])
        self.assertFalse(os.path.exists(self.out))

    def test_foreign_output_created_mid_run_is_not_deleted(self) -> None:
        # The initial check cannot see an --out another process creates later; a failure
        # before this run publishes must leave that foreign directory alone
        # (Codex finding stage-release.sh:152).
        self.fixture()
        tools = os.path.join(self.tmp, 'bin')
        os.mkdir(tools)
        real_install = shutil.which('install')
        flag = os.path.join(self.tmp, 'foreign-created')
        stub = os.path.join(tools, 'install')
        with open(stub, 'w', encoding='utf-8') as output:
            output.write(f'''#!/bin/sh
case " $* " in
  *p1-linux-x86_64*)
    if [ ! -e "{flag}" ]; then
      : > "{flag}"
      mkdir -p "{self.out}"
      printf 'foreign' > "{self.out}/keep"
      exit 73
    fi ;;
esac
exec '{real_install}' "$@"
''')
        os.chmod(stub, 0o755)
        with unittest.mock.patch.dict(os.environ, {'PATH': tools + ':' + os.environ['PATH']}):
            done = self.stage()
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertEqual(self.read(os.path.join(self.out, 'keep')), b'foreign')

    def test_bad_tmpdir_leaves_no_release_scratch(self) -> None:
        self.fixture()
        with unittest.mock.patch.dict(os.environ, {'TMPDIR': os.path.join(self.tmp, 'missing')}):
            done = self.stage()
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(any(n.startswith('.p1-release.') for n in os.listdir(self.tmp)))

    def test_publication_takes_an_exclusive_lock(self) -> None:
        # Two invocations replacing one --out must not interleave the directory swap with the
        # ownership-record write, or the final assets and record disagree (Codex finding
        # stage-release.sh:286). The run holds an exclusive flock for its whole duration.
        self.fixture()
        tools = os.path.join(self.tmp, 'bin')
        os.mkdir(tools)
        calls = os.path.join(self.tmp, 'flock-calls')
        real = shutil.which('flock') or '/usr/bin/flock'
        stub = os.path.join(tools, 'flock')
        with open(stub, 'w', encoding='utf-8') as output:
            output.write('#!/bin/sh\n'
                         f'printf \'%s\\n\' "$*" >> {calls}\n'
                         f'exec {real} "$@"\n')
        os.chmod(stub, 0o755)
        with unittest.mock.patch.dict(os.environ, {'PATH': tools + ':' + os.environ['PATH']}):
            done = self.stage()
        self.assertEqual(done.returncode, 0, done.stderr)
        with open(calls, encoding='utf-8') as handle:
            logged = handle.read()
        self.assertIn('-x 9', logged)

    def test_the_share_archive_carries_the_shipped_roots_and_the_module_set(self) -> None:
        data = self.fixture()

        done = self.stage()

        self.assertEqual(done.returncode, 0, done.stderr)
        files = self.members()
        roots = {name.split("/", 1)[0] for name in files}
        # The core catalog holds no account file since the owner's moved to their pack
        # (#853): `accounts/` is archived empty, and a root that has accounts fills it.
        self.assertEqual(roots, set(ROOTS) - {"accounts"})
        self.assertIn("modules/manifest.json", files)
        self.assertEqual(files["modules/packages/p1-fixture/p1-fixture.wasm"], data)
        for root in ("environments", "routes", "profiles"):
            self.assertTrue([name for name in files if name.startswith(root + "/")], root)
        # Only the component is shipped: none of the build's other files reaches the archive.
        for name in files:
            self.assertNotIn(name.rsplit("/", 1)[-1],
                             ("p1-module-fixture.wit", "p1-module-fixture.imports",
                              "p1-module-fixture.sha256", "p1-module-fixture.manifest.json"))

    def test_two_catalog_roots_merge_nested_files_without_tier_directories(self) -> None:
        self.fixture()
        extra = os.path.join(self.tmp, "connector")
        for directory in ("routes", "accounts", "profiles", "environments/extra"):
            write_file(os.path.join(extra, directory, "extra.toml"), b"extra catalog\n")
        done = self.stage("--catalog-root", ".", "--catalog-root", extra)
        self.assertEqual(done.returncode, 0, done.stderr)
        files = self.members()
        for directory in ("routes", "accounts", "profiles", "environments/extra"):
            self.assertEqual(files[directory + "/extra.toml"], b"extra catalog\n")
        self.assertIn("environments/default/environment.toml", files)
        self.assertFalse(any(name.startswith("connector/") for name in files))

    def test_duplicate_catalog_path_names_both_sources_and_leaves_no_out(self) -> None:
        self.fixture()
        extra = os.path.join(self.tmp, "connector")
        relative = "environments/default/environment.toml"
        write_file(os.path.join(extra, relative), b"duplicate\n")
        done = self.stage("--catalog-root", ".", "--catalog-root", extra)
        self.assert_no_out(done, "duplicate catalog path " + relative)
        self.assertIn(os.path.join(extra, relative), done.stderr)
        self.assertIn(os.path.join(os.path.dirname(HERE), ".", relative), done.stderr)

    # ---- the shipped roots (ADR-0217) and the part owner ---------------------------

    def catalog(self, root: str) -> set[str]:
        """The catalog files a source root holds, as share-relative paths."""
        base = os.path.join(os.path.dirname(HERE), root)
        found = set()
        for directory in ("environments", "routes", "profiles", "accounts"):
            for path, _, files in os.walk(os.path.join(base, directory)):
                found.update(os.path.relpath(os.path.join(path, name), base) for name in files)
        return found

    def shipped_roots(self) -> list[str]:
        """catalog-roots.txt, the list stage-release.sh and the anchor read."""
        text = self.read(os.path.join(os.path.dirname(HERE), "catalog-roots.txt")).decode()
        return [line for line in text.splitlines() if line and not line.startswith("#")]

    def test_the_share_composes_the_shipped_roots_and_the_owner_part(self) -> None:
        self.fixture()
        roots = self.shipped_roots()
        self.assertEqual(roots[0], ".")
        self.assertIn("connectors/anthropic", roots)
        shipped = set().union(*(self.catalog(root) for root in roots))
        owner = self.catalog("packs/owner")
        self.assertTrue(shipped and owner)
        for args, expected in (((), shipped), (("--parts", ""), shipped),
                               (("--parts", "owner"), shipped | owner)):
            with self.subTest(args=args):
                shutil.rmtree(self.out, ignore_errors=True)
                done = self.stage(*args)
                self.assertEqual(done.returncode, 0, done.stderr)
                staged = {name for name in self.members() if not name.startswith("modules/")}
                self.assertEqual(staged, expected)
        # The connectors' routes are the shipped roots', the owner's environments and accounts
        # the part's.
        self.assertIn("routes/glm-subscription.toml", self.catalog("connectors/zai"))
        self.assertIn("routes/anthropic.toml", self.catalog("connectors/anthropic"))
        self.assertNotIn("routes/anthropic.toml", self.catalog("."))
        self.assertIn("environments/claude/environment.toml", owner)
        self.assertIn("accounts/zai.toml", owner)

    def test_a_connector_package_ships_only_with_its_connector(self) -> None:
        # ADR-0214, ADR-0217: the connectors' packages are built beside p1's own, under the crate
        # names of connectors/<vendor>/components/*; a share ships those of the connectors whose
        # roots it composes.
        self.fixture()
        for args, glm, claude in ((("--catalog-root", "."), False, False),
                                  (("--catalog-root", ".", "--catalog-root", "connectors/zai"),
                                   True, False),
                                  ((), True, True), (("--parts", "owner"), True, True)):
            with self.subTest(args=args):
                shutil.rmtree(self.out, ignore_errors=True)
                done = self.stage(*args)
                self.assertEqual(done.returncode, 0, done.stderr)
                members = self.members()
                self.assertIn("modules/packages/p1-fixture/p1-fixture.wasm", members)
                self.assertEqual(
                    "modules/packages/connectors-glm/connectors-glm.wasm" in members, glm,
                    sorted(members))
                self.assertEqual(
                    "modules/packages/connectors-claude/connectors-claude.wasm" in members,
                    claude, sorted(members))
        # The build outputs are read, never changed.
        self.assertTrue(os.path.isdir(os.path.join(self.modules, "p1-connector-usage-glm")))

    def test_a_stray_output_package_is_refused_whatever_the_roots(self) -> None:
        self.fixture()
        stray = "p1-module-claude-code-oauth"
        self.package(stray, "p1/claude-code-oauth", b"stale component bytes\n")
        for args in ((), ("--catalog-root", ".")):
            with self.subTest(args=args):
                self.assert_no_out(self.stage(*args), "unknown module package " + stray)

    def test_a_shipped_connector_with_missing_packages_is_refused(self) -> None:
        self.fixture()
        crates = self.connector_packages()
        for missing in (["p1-connector-usage-glm"], crates):
            with self.subTest(missing=missing):
                self.connector_packages()
                for crate in missing:
                    shutil.rmtree(os.path.join(self.modules, crate))
                done = self.stage()
                self.assert_no_out(done, "missing module packages for selected parts")
                for crate in missing:
                    self.assertIn(crate, done.stderr)

    def test_an_unknown_part_or_two_root_lists_are_usage_errors(self) -> None:
        self.fixture()
        for args, message in ((("--parts", "owner,brain"), "unknown part brain"),
                              (("--parts", "subscriptions"), "unknown part subscriptions"),
                              (("--parts", "owner", "--catalog-root", "."), "give one"),
                              (("--asset-name", "p1-bare"), "unknown argument: --asset-name")):
            with self.subTest(args=args):
                done = self.stage(*args)
                self.assertEqual(done.returncode, 2, done.stderr)
                self.assertIn(message, done.stderr)
                self.assertFalse(os.path.exists(self.out))

    def test_missing_explicit_catalog_root_is_refused(self) -> None:
        self.fixture()
        self.assert_no_out(self.stage("--catalog-root", os.path.join(self.tmp, "missing")),
                           "not a directory")

    def test_the_manifest_binds_every_shipped_file(self) -> None:
        data = self.fixture()

        # The repository root alone: the connectors' packages are another test's.
        self.assertEqual(self.stage("--catalog-root", ".").returncode, 0)

        manifest = self.manifest()
        self.assertEqual(manifest["format"], "p1-release-manifest/1")
        self.assertEqual(manifest["commit"], COMMIT)
        self.assertEqual(manifest["tag"], TAG)
        self.assertEqual(manifest["environment_locks"], [])
        # The compiled copy the shipped binary wrote beside the component (ADR-0113).
        compiled = b"compiled p1-fixture.wasm\n"
        self.assertEqual(
            manifest["packages"],
            [{"path": "packages/p1-fixture/p1-fixture.cwasm",
              "sha256": sha256(compiled), "size": len(compiled)},
             {"path": "packages/p1-fixture/p1-fixture.wasm",
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
              "variant": "default",
              "precompiled": {"path": "packages/p1-fixture/p1-fixture.cwasm",
                              "digest": "sha256:" + sha256(compiled)}}],
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
        self.package("p1-module-shell", "p1/other", b"other\n")

        self.assertEqual(self.stage("--catalog-root", ".").returncode, 0)

        manifest = self.manifest()
        self.assertEqual([entry["name"] for entry in manifest["components"]],
                         ["p1/fixture", "p1/other"])
        self.assertEqual(sorted(member for member in self.members()
                                if member.startswith("modules/packages/")),
                         ["modules/packages/p1-fixture/p1-fixture.cwasm",
                          "modules/packages/p1-fixture/p1-fixture.wasm",
                          "modules/packages/p1-other/p1-other.cwasm",
                          "modules/packages/p1-other/p1-other.wasm"])

    # ---- refusals ----------------------------------------------------------------

    def test_a_development_manifest_at_the_top_is_not_a_package(self) -> None:
        # scripts/build-modules.sh writes the development manifest at the top of the same
        # build-outputs directory this script stages from (BLOCKERS S3-B6, D080): it sits
        # beside the packages and must be skipped, never packed as one.
        data = self.fixture()
        write_file(os.path.join(self.modules, "manifest.json"),
                   b'{"format": "p1-release-manifest/1", "components": []}\n')

        done = self.stage()

        self.assertEqual(done.returncode, 0, done.stderr)
        files = self.members()
        self.assertEqual(files["modules/packages/p1-fixture/p1-fixture.wasm"], data)
        # The only manifest.json in the archive is the release manifest this script writes.
        self.assertEqual(
            sorted(name for name in files if name.rsplit("/", 1)[-1] == "manifest.json"),
            ["modules/manifest.json"],
        )

    def test_any_other_top_level_regular_file_is_not_a_package_directory(self) -> None:
        # The skip is by name, for the development manifest only; every other top-level entry
        # keeps the package-directory check.
        self.fixture()
        write_file(os.path.join(self.modules, "stray.txt"), b"stray\n")

        self.assert_no_out(self.stage(), "not a package directory")

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

    def test_a_binary_that_cannot_precompile_is_refused_and_leaves_no_out(self) -> None:
        # ADR-0113: every shipped package carries the compiled copy the shipped binary wrote.
        self.fixture()
        write_file(self.binary, b"#!/bin/sh\nexit 3\n", 0o755)
        self.assert_no_out(self.stage(), "cannot compile the staged packages ahead of time")

    def test_no_module_packages_is_refused_and_leaves_no_out(self) -> None:
        self.assert_no_out(self.stage("--catalog-root", "."), "no module packages to ship")

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

        self.assertEqual(self.stage("--catalog-root", ".").returncode, 0)

        members = set(self.members())
        modules_members = {name for name in members if name.startswith("modules/")}
        self.assertEqual(
            modules_members,
            {"modules/manifest.json", "modules/packages/p1-fixture/p1-fixture.wasm",
             "modules/packages/p1-fixture/p1-fixture.cwasm"},
        )
        for leaked in ("modules/toolchain.pins", "modules/Cargo.toml", "modules/Cargo.lock",
                       "modules/capabilities.toml", "modules/wit"):
            self.assertNotIn(leaked, members, leaked)
        self.assertFalse([name for name in members if "p1-module-fixture/src" in name])


if __name__ == "__main__":
    unittest.main()
