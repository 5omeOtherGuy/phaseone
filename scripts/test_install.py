#!/usr/bin/env python3
"""Unit tests for scripts/install.sh — stdlib unittest, temp dirs only, no network.

    python3 scripts/test_install.py [-v]

A fixture "release" directory stands in for the GitHub release: a fake `p1` shell
script that answers `--version` and `login --list`, a share tarball with the three
top-level directories, and the two sha256 files. Stub `gh` and `curl` on PATH copy
from that fixture and record their argv and URLs; a stub `cargo` fakes a local release
build. Every home, config dir and prefix is a temp dir, so the machine's real
`~/.config/p1` is never touched.

`CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS`, `findmnt`, and `df` make the `--local` target,
build limits, filesystem, and free-space checks hermetic without touching a real home.
"""
from __future__ import annotations

import hashlib
import io
import json
import os
import re
import select
import shutil
import signal
import stat
import subprocess
import tarfile
import tempfile
import unittest

SCRIPTS = os.path.dirname(os.path.abspath(__file__))
INSTALL = os.path.join(SCRIPTS, "install.sh")
UPDATE = os.path.join(SCRIPTS, "update.sh")
RELEASE_WORKFLOW = os.path.join(os.path.dirname(SCRIPTS), ".github", "workflows", "release.yml")

# The interpreter by absolute path, so a test PATH controls only what install.sh sees.
BASH = shutil.which("bash") or "/bin/bash"

# The directories a test PATH keeps for sh, tar, sha256sum, cp, mktemp and friends.
SYSTEM_PATH = "/usr/bin:/bin"

# install.sh's external commands, symlinked into a farm of its own: a PATH built from
# the farm carries no `gh`, which is how the curl fallback is reached deterministically.
FARM_TOOLS = ("mktemp", "sha256sum", "cut", "awk", "tar", "gzip", "cp", "mv", "mkdir",
              "rm", "chmod", "basename", "dirname", "env", "cat", "python3",
              "findmnt", "df", "realpath")

VERSION_LINE = "p1 0.0.1 (deadbeef0000 2026-09-24)"

# The installed binary the release channel serves. Besides `--version` and `login --list` it
# answers the `modules verify --integrity-only --root DIR` call install.sh makes before it
# touches the prefix: P1_VERIFY_MODE picks the outcome (the default verifies), and every call
# appends `self=`, `argv=`, the prefix's own marker and its `.p1.new.` count to
# P1_VERIFY_LOG — that record is how a test shows the argv, and that the call happened while
# the prefix still held the previous generation.
FAKE_P1 = """#!/bin/sh
case "$1" in
  --version) printf '%s\\n' "@VERSION@" ;;
  login) printf '%s\\n' "route  kind  source" "plain  api_key  p1 store only" ;;
  modules)
    if [ -n "${P1_VERIFY_LOG:-}" ]; then
      {
        printf 'self=%s\\n' "$0"
        printf 'argv=%s\\n' "$*"
        printf 'prefix_marker=%s\\n' "$(cat "${P1_FAKE_PREFIX:-/nonexistent}/share/p1/environments/marker.txt" 2>/dev/null)"
        printf 'new_entries=%s\\n' "$(ls -A "${P1_FAKE_PREFIX:-/nonexistent}/bin" 2>/dev/null | grep -c '^\\.p1\\.new\\.')"
      } >>"$P1_VERIFY_LOG"
    fi
    case "${P1_VERIFY_MODE:-ok}" in
      ok) printf '%s\\n' "modules verify: 1 ok, 0 failed" ;;
      unlinked)
        printf '%s\\n' "UNLINKED workers-start (tools-tool)"
        printf '%s\\n' "modules verify: 1 ok, 0 failed" ;;
      report)
        # A report that reads like a failure while the exit status says verified: the
        # installer may not read the text (D069).
        printf '%s\\n' "tools-tool FAILED duplicate identity: sha256:00 is also tools-other"
        printf '%s\\n' "modules verify: 0 ok, 1 failed" ;;
      fail)
        printf '%s\\n' "tools-tool FAILED duplicate identity: sha256:00 is also tools-other" >&2
        printf '%s\\n' "modules verify: 0 ok, 1 failed"
        exit 1 ;;
      crash) kill -s SEGV $$ ;;
      flag)
        printf '%s\\n' 'unknown flag `--integrity-only`' >&2
        exit 2 ;;
      unusable)
        printf '%s\\n' 'p1: cannot run the staged binary' >&2
        exit 126 ;;
    esac
    ;;
  *) printf 'fake p1: %s\\n' "$*" ;;
esac
""".replace("@VERSION@", VERSION_LINE)

# A second published release, naming a different commit: `p1 --version` tells the two
# apart, which is how the installer recognizes the release it already has.
NEW_RELEASE = FAKE_P1.replace("fake p1", "new p1").replace("deadbeef0000", "cafebabe0000")

GH_STUB = """#!/bin/sh
printf '%s\\n' "$*" >> "$P1_GH_LOG"
if [ "$1" = "release" ] && [ "$2" = "view" ]; then
  # `gh release view --json tagName --jq .tagName`: the tag "latest" carries.
  cat "$P1_FIXTURE_RELEASE/latest-tag"
  exit 0
fi
dir=""
pattern=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dir) dir="$2"; shift 2 ;;
    --pattern) pattern="$2"; shift 2 ;;
    --repo) shift 2 ;;
    --clobber) shift ;;
    *) shift ;;
  esac
done
case "${P1_GH_FAIL:-}" in
  download) exit 7 ;;
  download-partial)
    # A gh that fails may still leave a truncated asset behind.
    printf 'truncated\\n' > "$dir/$pattern"
    exit 7 ;;
esac
cp "$P1_FIXTURE_RELEASE/$pattern" "$dir/$pattern"
"""

CURL_STUB = """#!/bin/sh
printf '%s\\n' "$*" >> "$P1_CURL_LOG"
out=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  */releases/latest)
    # The redirect target of releases/latest: what `-w '%{url_effective}'` prints.
    printf '%s\\n' "https://example.invalid/releases/tag/$(cat "$P1_FIXTURE_RELEASE/latest-tag")" ;;
  *)
    cp "$P1_FIXTURE_RELEASE/$(basename "$url")" "$out" ;;
esac
"""

CARGO_STUB = """#!/bin/sh
printf 'target=%s\\n' "$CARGO_TARGET_DIR" >> "$P1_CARGO_LOG"
printf 'jobs=%s\\n' "${CARGO_BUILD_JOBS:-unset}" >> "$P1_CARGO_LOG"
printf 'wrapper=%s\\n' "${RUSTC_WRAPPER:-unset}" >> "$P1_CARGO_LOG"
printf 'argv=%s\\n' "$*" >> "$P1_CARGO_LOG"
mkdir -p "$CARGO_TARGET_DIR/release"
cp "$P1_FIXTURE_RELEASE/p1-linux-x86_64" "$CARGO_TARGET_DIR/release/p1"
chmod 0755 "$CARGO_TARGET_DIR/release/p1"
"""

ASSETS = ("p1-linux-x86_64", "p1-linux-x86_64.sha256",
          "p1-share.tar.gz", "p1-share.tar.gz.sha256")

# A manifest commit is the released source commit: 40 lowercase hex characters.
MANIFEST_COMMIT = "0123456789abcdef0123456789abcdef01234567"


class InstallTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="install-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.home = self.mkdir("home")
        self.config = self.mkdir("config")
        self.release = self.mkdir("release")
        self.stub_dir = self.mkdir("stubs")
        self.curl_dir = self.mkdir("stubs-curl")
        self.farm_dir = self.make_farm()
        self.prefix = os.path.join(self.dir, "prefix")
        self.gh_log = os.path.join(self.dir, "gh.log")
        self.curl_log = os.path.join(self.dir, "curl.log")
        self.cargo_log = os.path.join(self.dir, "cargo.log")
        self.verify_log = os.path.join(self.dir, "verify.log")
        self.publish(marker="one", binary=FAKE_P1)
        self.stub("gh", GH_STUB)
        self.stub("curl", CURL_STUB)
        # The same curl stub in a directory of its own: `gh` is genuinely absent there.
        self.stub("curl", CURL_STUB, directory=self.curl_dir)

    def stub_local_fs(self, filesystem: str = "ext4", free: int = 12884901888) -> None:
        self.stub("findmnt", f"#!/bin/sh\nprintf '%s\\n' {filesystem!r}\n", directory=self.stub_dir)
        self.stub("df", f"#!/bin/sh\nprintf '%s\\n' 'Filesystem 1-blocks Used Available Capacity Mounted on' 'stub 20000000000 0 {free} 0% /stub'\n", directory=self.stub_dir)

    def assert_local_refused(self, done: subprocess.CompletedProcess, message: str) -> None:
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn(message, done.stderr)
        self.assertEqual(self.log(self.cargo_log), "")
        self.assert_nothing_installed(self.prefix)

    # --- fixture and helpers ----------------------------------------------

    def mkdir(self, name: str) -> str:
        path = os.path.join(self.dir, name)
        os.makedirs(path, exist_ok=True)
        return path

    def stub(self, name: str, text: str, directory: str | None = None) -> str:
        path = os.path.join(directory or self.stub_dir, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        os.chmod(path, 0o755)
        return path

    def make_farm(self) -> str:
        """A PATH directory holding only the tools install.sh runs: no `gh` in it."""
        farm = self.mkdir("farm")
        for tool in FARM_TOOLS:
            found = shutil.which(tool)
            self.assertIsNotNone(found, tool)
            os.symlink(found, os.path.join(farm, tool))
        return farm

    def no_gh(self) -> dict:
        """PATH overrides that drop `gh`: the curl fallback's world."""
        return {"PATH": self.curl_dir + ":" + self.farm_dir}

    @staticmethod
    def digest(path: str) -> str:
        with open(path, "rb") as handle:
            return hashlib.file_digest(handle, "sha256").hexdigest()

    def write_sum(self, asset: str) -> None:
        path = os.path.join(self.release, asset)
        with open(path + ".sha256", "w", encoding="utf-8") as handle:
            handle.write(f"{self.digest(path)}  {asset}\n")

    def make_share_tarball(self, marker: str, modules: dict | None = None) -> None:
        """Top-level environments/, routes/ and profiles/, plus an optional modules/ tree."""
        path = os.path.join(self.release, "p1-share.tar.gz")
        with tarfile.open(path, "w:gz") as archive:
            for top in ("environments", "routes", "profiles"):
                data = f"{top} {marker}\n".encode()
                info = tarfile.TarInfo(f"{top}/marker.txt")
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
            if modules is not None:
                self.add_modules(archive, modules)

    def add_modules(self, archive: tarfile.TarFile, spec: dict) -> None:
        """Write a modules/ tree: manifest.json, packages/<files>, and raw extras."""
        manifest = spec.get("manifest")
        if manifest is not None:
            if isinstance(manifest, (dict, list)):
                text = json.dumps(manifest, sort_keys=True, indent=2).encode()
            elif isinstance(manifest, str):
                text = manifest.encode()
            else:
                text = manifest
            info = tarfile.TarInfo("modules/manifest.json")
            info.size = len(text)
            archive.addfile(info, io.BytesIO(text))
        for name, data in spec.get("files", {}).items():
            info = tarfile.TarInfo("modules/packages/" + name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        for info, fileobj in spec.get("members", []):
            archive.addfile(info, fileobj)

    @staticmethod
    def tar_member(name: str, data: bytes = b"", kind: bytes = tarfile.REGTYPE,
                   linkname: str = "") -> tuple:
        info = tarfile.TarInfo(name)
        info.type = kind
        info.linkname = linkname
        info.size = len(data) if kind in (tarfile.REGTYPE, tarfile.AREGTYPE) else 0
        return info, (io.BytesIO(data) if info.size else None)

    def manifest(self, packages: list, *, binary_sha: str | None = None, **overrides) -> dict:
        """A valid p1-release-manifest/1 whose native pin names the published binary."""
        if binary_sha is None:
            binary_sha = self.digest(os.path.join(self.release, "p1-linux-x86_64"))
        manifest = {
            "format": "p1-release-manifest/1",
            "commit": MANIFEST_COMMIT,
            "tag": None,
            "native": {"asset": "p1-linux-x86_64", "sha256": binary_sha},
            "toolchain": {"rustc": None, "cargo": None, "wasm_target": None,
                          "wasm_tools": None, "wit_bindgen": None},
            "runtime": {"wasmtime": None, "wasmtime_features": None},
            "wit": [],
            "schemas": [],
            "packages": packages,
            "components": [],
            "environment_locks": [],
        }
        for key, value in overrides.items():
            if value is None:
                manifest.pop(key, None)
            else:
                manifest[key] = value
        return manifest

    @staticmethod
    def package_entry(name: str, data: bytes, **overrides) -> dict:
        """A manifest packages[] entry for data, relative to the archive's modules/."""
        entry = {"path": "packages/" + name,
                 "sha256": hashlib.sha256(data).hexdigest(),
                 "size": len(data)}
        entry.update(overrides)
        return entry

    def module_spec(self, packages: list, files: dict, **manifest_overrides) -> callable:
        """A publish() modules= callable whose manifest pins the published binary."""
        def build(binary_sha: str) -> dict:
            return {"manifest": self.manifest(packages, binary_sha=binary_sha,
                                              **manifest_overrides),
                    "files": files}
        return build

    def publish(self, marker: str = "one", binary: str = FAKE_P1, modules=None) -> None:
        """(Re)write the whole fixture release, checksums included."""
        with open(os.path.join(self.release, "p1-linux-x86_64"), "w", encoding="utf-8") as h:
            h.write(binary)
        binary_sha = self.digest(os.path.join(self.release, "p1-linux-x86_64"))
        spec = modules(binary_sha) if callable(modules) else modules
        self.make_share_tarball(marker, spec)
        for asset in ASSETS:
            if not asset.endswith(".sha256"):
                self.write_sum(asset)
        # The tag the channel marks latest names the commit the published binary prints,
        # so a re-published fixture is a *different* latest release.
        sha = re.search(r"\(([0-9a-f]+) ", binary).group(1)
        with open(os.path.join(self.release, "latest-tag"), "w", encoding="utf-8") as h:
            h.write(f"main-{sha}\n")

    def env(self, **overrides) -> dict:
        env = {
            "HOME": self.home,
            "XDG_CONFIG_HOME": self.config,
            "PATH": self.stub_dir + ":" + SYSTEM_PATH,
            "LC_ALL": "C",
            "P1_FIXTURE_RELEASE": self.release,
            "P1_REPO": "test/repo",
            "P1_RELEASE_BASE_URL": "https://example.invalid/test/repo/releases",
            "P1_GH_LOG": self.gh_log,
            "P1_CURL_LOG": self.curl_log,
            "P1_CARGO_LOG": self.cargo_log,
            # The staged binary's own verification: where it records itself, and the prefix it
            # reports the state of at the moment it ran.
            "P1_VERIFY_LOG": self.verify_log,
            "P1_FAKE_PREFIX": self.prefix,
        }
        env.update(overrides)
        return env

    def start_verify_record(self) -> None:
        """One record per invocation, so a call assertion speaks about this run alone."""
        if os.path.exists(self.verify_log):
            os.remove(self.verify_log)

    def run_install(self, *args: str, **overrides) -> subprocess.CompletedProcess:
        self.start_verify_record()
        return subprocess.run([BASH, INSTALL, *args], env=self.env(**overrides),
                              capture_output=True, text=True)

    def run_script(self, script: str, *args: str) -> subprocess.CompletedProcess:
        self.start_verify_record()
        return subprocess.run([BASH, script, *args], env=self.env(),
                              capture_output=True, text=True)

    def log(self, path: str) -> str:
        if not os.path.isfile(path):
            return ""
        with open(path, encoding="utf-8") as handle:
            return handle.read()

    def gh_downloads(self) -> list:
        """The `gh release download` calls, without a `gh release view` tag lookup."""
        return [line for line in self.log(self.gh_log).splitlines() if " download " in line]

    def curl_downloads(self) -> list:
        """The asset downloads, without a `releases/latest` redirect probe."""
        return [line for line in self.log(self.curl_log).splitlines() if "/download/" in line]

    def read(self, path: str) -> str:
        with open(path, encoding="utf-8") as handle:
            return handle.read()

    def assert_installed(self, prefix: str) -> None:
        binary = os.path.join(prefix, "bin", "p1")
        self.assertTrue(os.path.isfile(binary), f"no {binary}")
        self.assertEqual(stat.S_IMODE(os.stat(binary).st_mode), 0o755, binary)
        self.assertEqual(self.read(binary), FAKE_P1)
        for top in ("environments", "routes", "profiles"):
            self.assertTrue(os.path.isdir(os.path.join(prefix, "share", "p1", top)), top)
        installer = os.path.join(prefix, "share", "p1", "install.sh")
        self.assertEqual(self.read(installer), self.read(INSTALL))
        wrapper = os.path.join(prefix, "bin", "p1-update")
        self.assertTrue(os.path.isfile(wrapper), wrapper)
        self.assertEqual(stat.S_IMODE(os.stat(wrapper).st_mode), 0o755, wrapper)

    def assert_nothing_installed(self, prefix: str) -> None:
        self.assertFalse(os.path.exists(os.path.join(prefix, "bin", "p1")))
        # No debris beside the prefix either.
        share = os.path.join(prefix, "share")
        if os.path.isdir(share):
            self.assertEqual([name for name in os.listdir(share) if name.startswith(".p1")],
                             [], os.listdir(share))

    def snapshot(self, path: str) -> dict:
        """Every entry below path, so a refusal can be shown to leave the prefix untouched."""
        if not os.path.exists(path):
            return {}
        entries = {}
        for root, dirs, files in os.walk(path):
            for name in sorted(dirs) + sorted(files):
                full = os.path.join(root, name)
                key = os.path.relpath(full, path)
                if os.path.islink(full):
                    entries[key] = ("symlink", os.readlink(full))
                elif os.path.isdir(full):
                    entries[key] = ("dir", stat.S_IMODE(os.stat(full).st_mode))
                else:
                    with open(full, "rb") as handle:
                        entries[key] = ("file", stat.S_IMODE(os.stat(full).st_mode),
                                        handle.read())
        return entries

    def verify_runs(self) -> list:
        """The staged binary's `modules verify` calls, one dict per invocation."""
        runs: list = []
        for line in self.log(self.verify_log).splitlines():
            if not line:
                continue
            key, _, value = line.partition("=")
            if key == "self":
                runs.append({"self": value})
            elif runs:
                runs[-1][key] = value
        return runs

    def assert_verify_never_called(self) -> None:
        """The inline python check refused, so the staged binary was never asked to verify."""
        self.assertEqual(self.log(self.verify_log), "",
                         "the inline module check refused, so `modules verify` must not have run")

    def assert_refused_untouched(self, done: subprocess.CompletedProcess, message: str,
                                 before: dict | None = None) -> None:
        """A verification failure names the problem, ends with the promise and changes nothing.

        Every caller is refused by the inline python check in `verify_modules`, which runs
        before the staged binary is asked anything, so the staged binary must not have run."""
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertIn(message, done.stderr)
        self.assertIn("nothing installed", done.stderr)
        self.assert_verify_never_called()
        if before is None:
            # The prefix did not exist before the refusal, so the refusal must not create it.
            self.assertFalse(os.path.exists(self.prefix), f"a refusal created {self.prefix}")
        else:
            self.assertEqual(self.snapshot(self.prefix), before)

    def assert_binary_refused_untouched(self, done: subprocess.CompletedProcess,
                                        before: dict | None = None) -> None:
        """The staged binary refused the set, and the prefix did not move for it.

        The refusal is the binary's exit status alone; the argv it recorded shows the call went
        to the staged copy over the extracted stage, with the prefix still holding whatever it
        held before (no `.p1.new.` copy beside the installed binary)."""
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertIn("the staged binary refused the module set", done.stderr)
        self.assertIn("nothing installed", done.stderr)
        runs = self.verify_runs()
        self.assertGreaterEqual(len(runs), 1, "the staged binary was never asked")
        self.assertEqual(runs[-1]["new_entries"], "0", "the prefix was already being written")
        if before is None:
            self.assertFalse(os.path.exists(self.prefix), f"a refusal created {self.prefix}")
        else:
            self.assertEqual(self.snapshot(self.prefix), before)

    # --- the release channel ----------------------------------------------

    def test_latest_install_from_gh(self) -> None:
        done = self.run_install("--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        self.assertIn(VERSION_LINE, done.stdout)
        self.assertIn("plain  api_key", done.stdout)
        self.assertIn("is not on PATH", done.stderr)
        gh = self.gh_downloads()
        self.assertEqual(len(gh), 4, gh)
        for asset in ASSETS:
            self.assertTrue(any(asset in line for line in gh), (asset, gh))
        self.assertEqual(self.log(self.curl_log), "")
        self.assertEqual(self.read(os.path.join(self.prefix, "share", "p1",
                                                "environments", "marker.txt")),
                         "environments one\n")

    def test_default_prefix_is_home_local(self) -> None:
        done = self.run_install()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(os.path.join(self.home, ".local"))

    def test_curl_fallback_when_gh_is_missing(self) -> None:
        done = self.run_install("--prefix", self.prefix, **self.no_gh())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        self.assertEqual(self.log(self.gh_log), "")
        curl = self.log(self.curl_log)
        for asset in ASSETS:
            self.assertIn("https://example.invalid/test/repo/releases/latest/download/" + asset,
                          curl)

    def test_from_release_tag_reaches_gh(self) -> None:
        done = self.run_install("--from-release", "v9.9.9", "--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        for line in self.log(self.gh_log).splitlines():
            self.assertIn("v9.9.9", line)

    def test_gh_failure_falls_back_to_curl(self) -> None:
        done = self.run_install("--prefix", self.prefix, P1_GH_FAIL="download")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        self.assertEqual(len(self.gh_downloads()), 4)
        curl = self.log(self.curl_log)
        for asset in ASSETS:
            self.assertIn("https://example.invalid/test/repo/releases/latest/download/" + asset,
                          curl)

    def test_a_gh_that_left_a_truncated_asset_falls_back_to_curl(self) -> None:
        # gh failing *after* writing part of an asset: the partial file must be discarded
        # rather than handed to the checksum check, so the install still succeeds.
        done = self.run_install("--prefix", self.prefix, P1_GH_FAIL="download-partial")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        self.assertEqual(len(self.gh_downloads()), 4)
        curl = self.log(self.curl_log)
        for asset in ASSETS:
            self.assertIn("https://example.invalid/test/repo/releases/latest/download/" + asset,
                          curl)
        # The verified asset is the real one, not the truncated stub output.
        self.assertEqual(self.read(os.path.join(self.prefix, "bin", "p1")), FAKE_P1)

    def test_from_release_tag_reaches_curl(self) -> None:
        done = self.run_install("--from-release", "v9.9.9", "--prefix", self.prefix,
                                **self.no_gh())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        curl = self.log(self.curl_log)
        self.assertIn("https://example.invalid/test/repo/releases/download/v9.9.9/p1-linux-x86_64",
                      curl)
        self.assertNotIn("/latest/download/", curl)

    def test_repo_override_is_used(self) -> None:
        done = self.run_install("--prefix", self.prefix, P1_REPO="other/repo")
        self.assertEqual(done.returncode, 0, done.stderr)
        for line in self.log(self.gh_log).splitlines():
            self.assertIn("--repo other/repo", line)

    def test_unknown_argument_is_refused(self) -> None:
        done = self.run_install("--bogus", "--prefix", self.prefix)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("unknown argument", done.stderr)
        self.assert_nothing_installed(self.prefix)

    # --- release workflow ---------------------------------------------------

    def test_release_workflow_repairs_partial_publications_and_uses_matching_cache_prefix(self) -> None:
        workflow = self.read(RELEASE_WORKFLOW)
        self.assertIn("key: release-cargo-${{ runner.os }}-", workflow)
        self.assertIn("release-cargo-${{ runner.os }}-", workflow.split("restore-keys:", 1)[1])
        self.assertIn("required=(p1-linux-x86_64 p1-linux-x86_64.sha256 p1-share.tar.gz p1-share.tar.gz.sha256)", workflow)
        self.assertIn("gh release upload", workflow)
        self.assertIn("--clobber", workflow)
        self.assertIn("git ls-remote", workflow)
        # Nothing else creates the tag, so the create call must not require it to exist:
        # the flag that aborts on a tag that does not exist yet could never publish a
        # first release. The create call makes the tag itself, at the commit the
        # ls-remote block above verified.
        create = workflow.split("gh release create", 1)[1].split("--title", 1)[0]
        options = {line.strip().rstrip("\\").strip() for line in create.splitlines()}
        self.assertIn('--target "$target"', options)
        self.assertNotIn("--verify-tag", options)

    # --- checksums and refusals --------------------------------------------

    def test_sha_mismatch_installs_nothing_and_keeps_the_previous_install(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        binary = os.path.join(self.prefix, "bin", "p1")
        before = self.read(binary)

        # A changed binary whose published checksum was not updated: the download must be
        # refused before anything in the prefix is touched. --force skips the "already
        # installed" answer, which would otherwise never fetch the tampered asset.
        with open(os.path.join(self.release, "p1-linux-x86_64"), "w", encoding="utf-8") as h:
            h.write(FAKE_P1.replace("fake p1", "tampered p1"))
        done = self.run_install("--prefix", self.prefix, "--force")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("sha256 mismatch", done.stderr)
        self.assertIn("nothing installed", done.stderr)
        self.assertEqual(self.read(binary), before)
        self.assert_installed(self.prefix)
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "bin"))
                          if name.startswith(".")], [])
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "share"))
                          if name.startswith(".")], [])

    def test_share_sha_mismatch_is_refused(self) -> None:
        with open(os.path.join(self.release, "p1-share.tar.gz"), "ab") as handle:
            handle.write(b"junk")
        done = self.run_install("--prefix", self.prefix)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("p1-share.tar.gz: sha256 mismatch", done.stderr)
        self.assert_nothing_installed(self.prefix)

    def test_archive_traversal_absolute_path_link_and_escape_are_refused(self) -> None:
        cases = {
            "traversal": ("environments/../escaped", tarfile.REGTYPE),
            "absolute": ("/tmp/p1-install-escaped", tarfile.REGTYPE),
            "symlink": ("environments/escape", tarfile.SYMTYPE),
            "hardlink": ("environments/escape", tarfile.LNKTYPE),
            "outside": ("elsewhere/escape", tarfile.REGTYPE),
        }
        outside = os.path.join(self.dir, "escaped")
        for name, (member_name, kind) in cases.items():
            with self.subTest(name=name):
                path = os.path.join(self.release, "p1-share.tar.gz")
                with tarfile.open(path, "w:gz") as archive:
                    data = b"escaped\n"
                    info = tarfile.TarInfo(member_name)
                    info.type = kind
                    info.linkname = "../../outside" if kind == tarfile.LNKTYPE else "/tmp/target"
                    info.size = len(data) if kind == tarfile.REGTYPE else 0
                    archive.addfile(info, io.BytesIO(data) if info.size else None)
                self.write_sum("p1-share.tar.gz")
                done = self.run_install("--prefix", self.prefix)
                self.assertNotEqual(done.returncode, 0, done.stdout)
                self.assertIn("unsafe or invalid", done.stderr)
                self.assertFalse(os.path.exists(outside))
                self.assert_nothing_installed(self.prefix)

    def test_a_share_tarball_without_the_shipped_dirs_is_refused(self) -> None:
        path = os.path.join(self.release, "p1-share.tar.gz")
        with tarfile.open(path, "w:gz") as archive:
            data = b"nope\n"
            info = tarfile.TarInfo("elsewhere/marker.txt")
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        self.write_sum("p1-share.tar.gz")
        done = self.run_install("--prefix", self.prefix)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("unsafe or invalid", done.stderr)
        self.assert_nothing_installed(self.prefix)

    # --- the fourth root: modules/ -----------------------------------------

    def refusing_releases(self) -> dict:
        """Every refusal the module checks raise: name -> (modules spec, message fragment)."""
        binary_sha = self.digest(os.path.join(self.release, "p1-linux-x86_64"))
        content = b"content\n"
        return {
            "a missing package file": (
                {"manifest": self.manifest(
                    [self.package_entry("only.wasm", b"present\n"),
                     self.package_entry("gone.wasm", b"absent\n")], binary_sha=binary_sha),
                 "files": {"only.wasm": b"present\n"}},
                "missing from the archive: packages/gone.wasm"),
            "an extra package file not in the manifest": (
                {"manifest": self.manifest([self.package_entry("a.wasm", b"a\n")],
                                           binary_sha=binary_sha),
                 "files": {"a.wasm": b"a\n", "extra.wasm": b"extra\n"}},
                "not listed in the manifest: packages/extra.wasm"),
            "a corrupted package": (
                {"manifest": self.manifest([self.package_entry("a.wasm", b"original")],
                                           binary_sha=binary_sha),
                 "files": {"a.wasm": b"tampered"}},
                "packages/a.wasm: sha256 is"),
            "a package size mismatch": (
                {"manifest": self.manifest(
                    [self.package_entry("a.wasm", content, size=len(content) + 4)],
                    binary_sha=binary_sha),
                 "files": {"a.wasm": content}},
                "size is 8 bytes, the manifest says 12"),
            "a duplicate manifest entry": (
                {"manifest": self.manifest(
                    [self.package_entry("a.wasm", b"a\n"),
                     self.package_entry("a.wasm", b"a\n")], binary_sha=binary_sha),
                 "files": {"a.wasm": b"a\n"}},
                "duplicates packages[0]"),
            "a traversing manifest path": (
                {"manifest": self.manifest(
                    [{"path": "packages/../escaped.wasm", "sha256": "0" * 64, "size": 1}],
                    binary_sha=binary_sha),
                 "files": {}},
                "has an empty, '.' or '..' component"),
            "an absolute manifest path": (
                {"manifest": self.manifest(
                    [{"path": "/tmp/escaped.wasm", "sha256": "0" * 64, "size": 1}],
                    binary_sha=binary_sha),
                 "files": {}},
                "has no packages/ prefix"),
            "a symlink under modules": (
                {"manifest": None, "files": {},
                 "members": [self.tar_member("modules/link", kind=tarfile.SYMTYPE,
                                             linkname="/tmp/target")]},
                "unsafe archive member: 'modules/link'"),
            "an extra entry directly under modules": (
                {"manifest": None, "files": {},
                 "members": [self.tar_member("modules/toolchain.pins", b"RUST_MIN=1.82.0\n")]},
                "only manifest.json and packages/ are allowed"),
            "modules without a manifest": (
                {"manifest": None, "files": {"a.wasm": b"a\n"}},
                "modules/ has no regular manifest.json"),
            "a wrong format string": (
                {"manifest": self.manifest([], binary_sha=binary_sha,
                                           format="p1-release-manifest/2"), "files": {}},
                "format is 'p1-release-manifest/2'"),
            "a native digest mismatch": (
                {"manifest": self.manifest(
                    [], binary_sha=binary_sha,
                    native={"asset": "p1-linux-x86_64", "sha256": "f" * 64}), "files": {}},
                "does not match the verified binary"),
            "a cwasm package": (
                {"manifest": self.manifest([self.package_entry("cache.cwasm", b"blob\n")],
                                           binary_sha=binary_sha),
                 "files": {"cache.cwasm": b"blob\n"}},
                ".cwasm is never shipped"),
            "an unexpected top-level key": (
                {"manifest": self.manifest([], binary_sha=binary_sha, extra=True), "files": {}},
                "unexpected extra"),
            "a value of the wrong JSON type": (
                {"manifest": self.manifest([], binary_sha=binary_sha, components={}),
                 "files": {}},
                "components is object, not array"),
            "a manifest that is not valid JSON": (
                {"manifest": b"{not json", "files": {}},
                "is not valid UTF-8 JSON"),
        }

    def test_a_release_with_a_manifest_and_packages_installs_them_under_share_p1_modules(self) -> None:
        files = {"tool.wasm": b"tool bytes\n", "providers/route.wasm": b"route bytes\n"}
        packages = [self.package_entry(name, data) for name, data in sorted(files.items())]
        self.publish(modules=self.module_spec(packages, files))
        done = self.run_install("--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        modules_root = os.path.join(self.prefix, "share", "p1", "modules")
        self.assertEqual(json.loads(self.read(os.path.join(modules_root, "manifest.json")))
                         ["packages"], packages)
        for name, data in files.items():
            with open(os.path.join(modules_root, "packages", name), "rb") as handle:
                self.assertEqual(handle.read(), data, name)

    def test_a_missing_package_file_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a missing package file"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_an_extra_package_file_not_in_the_manifest_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["an extra package file not in the manifest"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_corrupted_package_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a corrupted package"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_package_size_mismatch_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a package size mismatch"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_duplicate_manifest_entry_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a duplicate manifest entry"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_traversing_manifest_path_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a traversing manifest path"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_an_absolute_manifest_path_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["an absolute manifest path"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_symlink_under_modules_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a symlink under modules"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_an_extra_entry_directly_under_modules_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["an extra entry directly under modules"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_modules_without_a_manifest_json_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["modules without a manifest"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_wrong_format_string_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a wrong format string"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_native_digest_mismatch_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a native digest mismatch"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_cwasm_package_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a cwasm package"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_manifest_with_an_unexpected_key_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["an unexpected top-level key"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_manifest_with_a_value_of_the_wrong_json_type_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a value of the wrong JSON type"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_a_manifest_that_is_not_valid_json_is_refused(self) -> None:
        spec, fragment = self.refusing_releases()["a manifest that is not valid JSON"]
        self.publish(modules=spec)
        self.assert_refused_untouched(self.run_install("--prefix", self.prefix), fragment)

    def test_every_module_refusal_leaves_an_existing_prefix_byte_identical(self) -> None:
        # A refusal may come after an install already exists; then not one byte may move.
        files = {"tool.wasm": b"tool v1\n"}
        packages = [self.package_entry("tool.wasm", files["tool.wasm"])]
        self.publish(marker="one", modules=self.module_spec(packages, files))
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        before = self.snapshot(self.prefix)
        for name, (spec, fragment) in self.refusing_releases().items():
            with self.subTest(name=name):
                self.publish(marker="one", modules=spec)
                done = self.run_install("--prefix", self.prefix, "--force")
                self.assert_refused_untouched(done, fragment, before)

    def test_a_manifest_tag_that_disagrees_with_from_release_is_refused(self) -> None:
        def modules(binary_sha: str) -> dict:
            return {"manifest": self.manifest([], binary_sha=binary_sha,
                                              tag="main-cafebabe0000"),
                    "files": {}}
        self.publish(modules=modules)
        done = self.run_install("--from-release", "v9.9.9", "--prefix", self.prefix)
        self.assert_refused_untouched(done, "does not match the requested release 'v9.9.9'")

    def test_a_manifest_tag_matching_from_release_installs(self) -> None:
        def modules(binary_sha: str) -> dict:
            return {"manifest": self.manifest([], binary_sha=binary_sha, tag="v9.9.9"),
                    "files": {}}
        self.publish(modules=modules)
        done = self.run_install("--from-release", "v9.9.9", "--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(os.path.isdir(os.path.join(self.prefix, "share", "p1", "modules")))

    def test_an_interrupted_commit_restores_the_previous_release_including_its_modules(self) -> None:
        files = {"tools/tool.wasm": b"tool v1\n"}
        packages = [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])]
        self.publish(marker="one", modules=self.module_spec(packages, files))
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        paths = {
            "binary": os.path.join(self.prefix, "bin", "p1"),
            "updater": os.path.join(self.prefix, "bin", "p1-update"),
            "share": os.path.join(self.prefix, "share", "p1", "environments", "marker.txt"),
            "module": os.path.join(self.prefix, "share", "p1", "modules", "packages",
                                   "tools", "tool.wasm"),
            "manifest": os.path.join(self.prefix, "share", "p1", "modules", "manifest.json"),
        }
        before = {name: self.read(path) for name, path in paths.items()}
        new_files = {"tools/tool.wasm": b"tool v2\n"}
        new_packages = [self.package_entry("tools/tool.wasm", new_files["tools/tool.wasm"])]
        self.publish(marker="two", binary=NEW_RELEASE,
                     modules=self.module_spec(new_packages, new_files))

        fail_dir = self.mkdir("fail-commit-modules")
        real_mv = shutil.which("mv")
        self.stub("mv", f"""#!/bin/sh
case \" $* \" in
  *".p1.new."*"/share/p1 "*) exit 71 ;;
esac
exec '{real_mv}' \"$@\"
""", directory=fail_dir)
        done = self.run_install("--prefix", self.prefix,
                                PATH=fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH)
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertIn("could not commit", done.stderr)
        for name, path in paths.items():
            self.assertEqual(self.read(path), before[name], name)
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "bin"))
                          if name.startswith(".p1")], [])
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "share"))
                          if name.startswith(".p1")], [])

    def test_an_old_release_without_modules_downgrades_over_one_with_modules(self) -> None:
        files = {"tool.wasm": b"tool v2\n"}
        packages = [self.package_entry("tool.wasm", files["tool.wasm"])]
        self.publish(marker="two", binary=NEW_RELEASE, modules=self.module_spec(packages, files))
        newer = self.run_install("--from-release", "v2.0.0", "--prefix", self.prefix)
        self.assertEqual(newer.returncode, 0, newer.stderr)
        self.assertTrue(os.path.isdir(os.path.join(self.prefix, "share", "p1", "modules")))

        # An older release has no modules/, so the whole share tree — modules included — is
        # replaced by the previous data and the downgrade succeeds.
        self.publish(marker="one", binary=FAKE_P1)
        older = self.run_install("--from-release", "v1.0.0", "--prefix", self.prefix)
        self.assertEqual(older.returncode, 0, older.stderr)
        self.assertEqual(self.read(os.path.join(self.prefix, "bin", "p1")), FAKE_P1)
        self.assertEqual(self.read(os.path.join(self.prefix, "share", "p1", ".p1-release")),
                         "v1.0.0\n")
        self.assertFalse(os.path.exists(os.path.join(self.prefix, "share", "p1", "modules")))

    # --- the staged binary's own verification (S1.6.1, D079) ----------------

    def test_the_staged_binary_verifies_the_extracted_set_before_the_prefix_moves(self) -> None:
        files = {"tools/tool.wasm": b"tool v1\n"}
        self.publish(marker="one", modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])], files))
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(len(self.verify_runs()), 1, "the staged binary was not asked once")

        new_files = {"tools/tool.wasm": b"tool v2\n"}
        self.publish(marker="two", binary=NEW_RELEASE, modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", new_files["tools/tool.wasm"])], new_files))
        second = self.run_install("--prefix", self.prefix)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn("modules verify: 1 ok, 0 failed", second.stdout)

        runs = self.verify_runs()
        self.assertEqual(len(runs), 1, runs)
        call = runs[-1]
        # The subcommand and the flag are the frozen ones, and --root names the directory the
        # share tarball was extracted into (the one that holds modules/).
        prefix_argv = "modules verify --integrity-only --root "
        self.assertTrue(call["argv"].startswith(prefix_argv), call)
        root = call["argv"][len(prefix_argv):]
        self.assertEqual(os.path.basename(root), "share", call)
        # The staged copy was run, not the installed binary: it sits beside the extracted
        # share, under the temporary stage, never under the prefix.
        self.assertEqual(os.path.dirname(call["self"]), os.path.dirname(root), call)
        self.assertEqual(os.path.basename(call["self"]), "p1", call)
        self.assertFalse(call["self"].startswith(self.prefix + os.sep), call)
        # Nothing under the prefix had moved when the binary ran: the installed release was
        # still the previous generation, with no .new copy written beside it.
        self.assertEqual(call["prefix_marker"], "environments one", call)
        self.assertEqual(call["new_entries"], "0", call)

    def test_a_release_without_modules_skips_the_staged_verification_in_one_line(self) -> None:
        self.publish()
        done = self.run_install("--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_verify_never_called()
        self.assertEqual([line for line in (done.stdout + done.stderr).splitlines()
                          if "modules/" in line],
                         ["p1 install: this release ships no modules/ — the staged binary is "
                          "not asked to verify a module set"], done.stdout + done.stderr)

    def test_a_duplicate_identity_is_refused_by_the_staged_binary(self) -> None:
        # S1's `--integrity-only` computes duplicate identity over `components`, never over
        # `packages`, and the inline check reads that list only for its type: two component
        # names over one digest pass the inline check, so the staged binary is the only guard
        # that could refuse them (the mode-driven fake stands in for its verdict). The bytes
        # are a minimal component, so a real verify reaches the identity check with no other
        # problem to report.
        component = b"\0asm\r\x00\x01\x00"
        files = {"tools/first.wasm": component, "tools/second.wasm": component}
        packages = [self.package_entry(name, data) for name, data in sorted(files.items())]
        digest = "sha256:" + hashlib.sha256(component).hexdigest()
        components = [
            {"name": "p1/first", "digest": digest, "path": "packages/tools/first.wasm",
             "kind": "tool", "world": "p1:module/tool@1.0.0", "protocol": "1.0",
             "capabilities": [], "variant": "default"},
            {"name": "p1/second", "digest": digest, "path": "packages/tools/second.wasm",
             "kind": "tool", "world": "p1:module/tool@1.0.0", "protocol": "1.0",
             "capabilities": [], "variant": "default"},
        ]
        self.publish(modules=self.module_spec(packages, files, components=components))
        done = self.run_install("--prefix", self.prefix, P1_VERIFY_MODE="fail")
        self.assert_binary_refused_untouched(done)
        self.assertIn("duplicate identity", done.stderr)
        self.assertIn("modules verify: 0 ok, 1 failed", done.stdout)
        self.assertEqual(len(self.verify_runs()), 1)

    def test_a_staged_binary_that_refuses_the_set_leaves_an_existing_install_untouched(self) -> None:
        files = {"tools/tool.wasm": b"tool v1\n"}
        self.publish(marker="one", modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])], files))
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        before = self.snapshot(self.prefix)

        new_files = {"tools/tool.wasm": b"tool v2\n"}
        self.publish(marker="two", binary=NEW_RELEASE, modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", new_files["tools/tool.wasm"])], new_files))
        done = self.run_install("--prefix", self.prefix, P1_VERIFY_MODE="fail")
        self.assert_binary_refused_untouched(done, before)
        self.assertEqual(self.verify_runs()[-1]["prefix_marker"], "environments one")

    def test_a_staged_binary_that_crashes_is_refused(self) -> None:
        files = {"tools/tool.wasm": b"tool bytes\n"}
        self.publish(modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])], files))
        done = self.run_install("--prefix", self.prefix, P1_VERIFY_MODE="crash")
        self.assert_binary_refused_untouched(done)
        self.assertIn("(exit 139)", done.stderr)

    def test_a_staged_binary_that_rejects_the_flag_is_refused(self) -> None:
        files = {"tools/tool.wasm": b"tool bytes\n"}
        self.publish(modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])], files))
        done = self.run_install("--prefix", self.prefix, P1_VERIFY_MODE="flag")
        self.assert_binary_refused_untouched(done)
        self.assertIn("unknown flag", done.stderr)
        self.assertIn("(exit 2)", done.stderr)

    def test_a_staged_binary_that_cannot_run_is_refused(self) -> None:
        files = {"tools/tool.wasm": b"tool bytes\n"}
        self.publish(modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])], files))
        done = self.run_install("--prefix", self.prefix, P1_VERIFY_MODE="unusable")
        self.assert_binary_refused_untouched(done)
        self.assertIn("(exit 126)", done.stderr)

    def test_an_unlinked_grant_with_exit_zero_installs(self) -> None:
        files = {"tools/tool.wasm": b"tool bytes\n"}
        self.publish(modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])], files))
        done = self.run_install("--prefix", self.prefix, P1_VERIFY_MODE="unlinked")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("UNLINKED workers-start", done.stdout)
        self.assert_installed(self.prefix)
        self.assertEqual(self.read(os.path.join(self.prefix, "share", "p1", "modules",
                                                "packages", "tools", "tool.wasm")),
                         "tool bytes\n")

    def test_a_report_that_reads_like_a_failure_but_exits_zero_installs(self) -> None:
        # D069: the exit status decides, never the text — a report that reads like a refusal
        # while the status says verified installs.
        files = {"tools/tool.wasm": b"tool bytes\n"}
        self.publish(modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", files["tools/tool.wasm"])], files))
        done = self.run_install("--prefix", self.prefix, P1_VERIFY_MODE="report")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("modules verify: 0 ok, 1 failed", done.stdout)
        self.assertTrue(os.path.isdir(os.path.join(self.prefix, "share", "p1", "modules")))

    # --- pinned generations: the set is renamed, never rewritten ------------

    def test_an_update_replaces_the_module_set_by_a_rename_not_in_place(self) -> None:
        """A session that opened the installed generation keeps reading those bytes."""
        old_files = {"tools/tool.wasm": b"tool v1\n"}
        self.publish(marker="one", modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", old_files["tools/tool.wasm"])], old_files))
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)

        packages = os.path.join(self.prefix, "share", "p1", "modules", "packages")
        tool = os.path.join(packages, "tools", "tool.wasm")
        open_file = open(tool, "rb")
        self.addCleanup(open_file.close)
        # Both inodes are held open, so the filesystem cannot hand either to the new tree: a
        # difference after the update proves the path was replaced, never written through.
        held_file_inode = os.fstat(open_file.fileno()).st_ino
        held_dir = os.open(packages, os.O_RDONLY | os.O_DIRECTORY)
        self.addCleanup(os.close, held_dir)
        held_dir_inode = os.fstat(held_dir).st_ino

        new_files = {"tools/tool.wasm": b"tool v2, a longer generation\n"}
        self.publish(marker="two", binary=NEW_RELEASE, modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", new_files["tools/tool.wasm"])], new_files))
        second = self.run_install("--prefix", self.prefix)
        self.assertEqual(second.returncode, 0, second.stderr)

        # The handle a running session holds still reads the generation it opened, at the very
        # inode it opened: the update renamed a fresh tree into place.
        self.assertEqual(open_file.read(), b"tool v1\n")
        self.assertEqual(os.fstat(open_file.fileno()).st_ino, held_file_inode)
        self.assertNotEqual(os.stat(tool).st_ino, held_file_inode)
        self.assertNotEqual(os.stat(packages).st_ino, held_dir_inode)
        with open(tool, "rb") as fresh:
            self.assertEqual(fresh.read(), b"tool v2, a longer generation\n")

    def test_a_failed_commit_restores_the_module_set_byte_for_byte(self) -> None:
        old_files = {"tools/tool.wasm": b"tool v1\n", "providers/route.wasm": b"route v1\n"}
        self.publish(marker="one", modules=self.module_spec(
            [self.package_entry(name, data) for name, data in sorted(old_files.items())],
            old_files))
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        modules = os.path.join(self.prefix, "share", "p1", "modules")
        before = self.snapshot(modules)

        new_files = {"tools/tool.wasm": b"tool v2\n"}
        self.publish(marker="two", binary=NEW_RELEASE, modules=self.module_spec(
            [self.package_entry("tools/tool.wasm", new_files["tools/tool.wasm"])], new_files))

        fail_dir = self.mkdir("fail-commit-module-set")
        real_mv = shutil.which("mv")
        self.stub("mv", f"""#!/bin/sh
case \" $* \" in
  *".p1.new."*"/share/p1 "*) exit 71 ;;
esac
exec '{real_mv}' \"$@\"
""", directory=fail_dir)
        done = self.run_install("--prefix", self.prefix,
                                PATH=fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH)
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertIn("could not commit", done.stderr)
        self.assertEqual(self.snapshot(modules), before)

    # --- the interpreter the archive checks need ---------------------------

    def test_a_python3_without_the_tarfile_data_filter_is_refused_by_name(self) -> None:
        old_dir = self.mkdir("old-python3")
        interpreter = self.stub("python3", "#!/bin/sh\nexit 2\n", directory=old_dir)
        done = self.run_install("--prefix", self.prefix, PATH=old_dir + ":" + SYSTEM_PATH)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("python3", done.stderr)
        self.assertIn("required", done.stderr)
        self.assertIn(interpreter, done.stderr)
        # Not a statement about the archive or the prefix, which were never reached.
        self.assertNotIn("unsafe or invalid", done.stderr)
        self.assertNotIn("refusing a prefix", done.stderr)
        self.assert_nothing_installed(self.prefix)

    def test_a_missing_python3_is_refused_by_name(self) -> None:
        os.remove(os.path.join(self.farm_dir, "python3"))
        done = self.run_install("--prefix", self.prefix, **self.no_gh())
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("no python3 on PATH", done.stderr)
        self.assertNotIn("unsafe or invalid", done.stderr)
        self.assertNotIn("refusing a prefix", done.stderr)
        self.assert_nothing_installed(self.prefix)

    # --- the user's own p1 config -----------------------------------------

    def test_prefix_equal_to_or_below_the_p1_config_tree_is_refused(self) -> None:
        auth_dir = os.path.join(self.config, "p1")
        os.makedirs(auth_dir)
        sentinel = os.path.join(auth_dir, "sentinel")
        with open(sentinel, "w", encoding="utf-8") as handle:
            handle.write("untouched\n")
        for prefix in (auth_dir, os.path.join(auth_dir, "tools")):
            with self.subTest(prefix=prefix):
                done = self.run_install("--prefix", prefix)
                self.assertNotEqual(done.returncode, 0, done.stdout)
                self.assertIn("refusing a prefix", done.stderr)
                self.assertEqual(self.read(sentinel), "untouched\n")
                self.assertEqual(os.listdir(auth_dir), ["sentinel"])

    def test_root_prefix_is_preserved(self) -> None:
        calls_log = os.path.join(self.dir, "python3.log")
        guard_dir = self.mkdir("root-guard")
        self.stub("python3", f"""#!/bin/sh
printf '%s\\n' "$*" >> '{calls_log}'
case "$*" in
  "-") exit 0 ;;
esac
exit 99
""", directory=guard_dir)
        done = self.run_install("--prefix", "/", PATH=guard_dir + ":" + SYSTEM_PATH)
        self.assertNotEqual(done.returncode, 0)
        # The capability probe runs first (argv just `-`); the guard call then receives the
        # rooted /bin and /share. Trimming "/" to "" would produce "bin" and "share".
        self.assertEqual(self.log(calls_log).splitlines()[-1],
                         f"- {self.config}/p1 //bin //share")

    def test_the_users_p1_config_is_byte_identical_afterwards(self) -> None:
        auth_dir = os.path.join(self.config, "p1")
        os.makedirs(auth_dir)
        auth = os.path.join(auth_dir, "auth.json")
        secret_marker = b'{"plain":{"api_key":"not-a-real-key"}}\n'
        with open(auth, "wb") as handle:
            handle.write(secret_marker)
        os.chmod(auth, 0o600)
        before = os.listdir(auth_dir)

        done = self.run_install("--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        with open(auth, "rb") as handle:
            self.assertEqual(handle.read(), secret_marker)
        self.assertEqual(stat.S_IMODE(os.stat(auth).st_mode), 0o600)
        self.assertEqual(os.listdir(auth_dir), before)
        self.assert_installed(self.prefix)

    # --- updates -----------------------------------------------------------

    def test_commit_failure_rolls_back_binary_share_and_updater(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        paths = {
            "binary": os.path.join(self.prefix, "bin", "p1"),
            "updater": os.path.join(self.prefix, "bin", "p1-update"),
            "share": os.path.join(self.prefix, "share", "p1", "environments", "marker.txt"),
        }
        before = {name: self.read(path) for name, path in paths.items()}
        self.publish(marker="two", binary=NEW_RELEASE)

        fail_dir = self.mkdir("fail-commit")
        real_mv = shutil.which("mv")
        self.stub("mv", f"""#!/bin/sh
case \" $* \" in
  *".p1.new."*"/share/p1 "*) exit 71 ;;
esac
exec '{real_mv}' \"$@\"
""", directory=fail_dir)
        done = self.run_install("--prefix", self.prefix,
                                PATH=fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH)
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertIn("could not commit", done.stderr)
        for name, path in paths.items():
            self.assertEqual(self.read(path), before[name], name)
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "bin"))
                          if name.startswith(".p1")], [])
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "share"))
                          if name.startswith(".p1")], [])

    def test_a_prefix_containing_colon_fully_rolls_back_a_commit_failure(self) -> None:
        prefix = os.path.join(self.dir, "prefix:with:colon")
        first = self.run_install("--prefix", prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        paths = {
            "binary": os.path.join(prefix, "bin", "p1"),
            "updater": os.path.join(prefix, "bin", "p1-update"),
            "share": os.path.join(prefix, "share", "p1", "environments", "marker.txt"),
        }
        before = {name: self.read(path) for name, path in paths.items()}
        self.publish(marker="two", binary=NEW_RELEASE)

        fail_dir = self.mkdir("fail-colon-commit")
        real_mv = shutil.which("mv")
        self.stub("mv", f"""#!/bin/sh
case \" $* \" in
  *".p1.new."*"/share/p1 "*) exit 71 ;;
esac
exec '{real_mv}' \"$@\"
""", directory=fail_dir)
        done = self.run_install("--prefix", prefix,
                                PATH=fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH)
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertIn("could not commit", done.stderr)
        for name, path in paths.items():
            self.assertEqual(self.read(path), before[name], name)
        self.assertEqual([name for name in os.listdir(os.path.join(prefix, "bin"))
                          if name.startswith(".p1")], [])
        self.assertEqual([name for name in os.listdir(os.path.join(prefix, "share"))
                          if name.startswith(".p1")], [])

    def test_staged_updater_failure_leaves_previous_install_intact(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        binary = self.read(os.path.join(self.prefix, "bin", "p1"))
        marker = self.read(os.path.join(self.prefix, "share", "p1",
                                        "environments", "marker.txt"))
        self.publish(marker="two", binary=NEW_RELEASE)

        fail_dir = self.mkdir("fail-updater")
        real_chmod = shutil.which("chmod")
        self.stub("chmod", f"""#!/bin/sh
case \"$1\" in
  0755) case \"$2\" in */.p1-update.new.*) exit 72 ;; esac ;;
esac
exec '{real_chmod}' \"$@\"
""", directory=fail_dir)
        done = self.run_install("--prefix", self.prefix,
                                PATH=fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH)
        self.assertNotEqual(done.returncode, 0, done.stdout)
        self.assertEqual(self.read(os.path.join(self.prefix, "bin", "p1")), binary)
        self.assertEqual(self.read(os.path.join(self.prefix, "share", "p1",
                                                "environments", "marker.txt")), marker)

    def test_a_signal_during_the_commit_rolls_back_and_ends_the_script(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        paths = {
            "binary": os.path.join(self.prefix, "bin", "p1"),
            "updater": os.path.join(self.prefix, "bin", "p1-update"),
            "share": os.path.join(self.prefix, "share", "p1", "environments", "marker.txt"),
        }
        before = {name: self.read(path) for name, path in paths.items()}
        self.publish(marker="two", binary=NEW_RELEASE)

        # The commit is parked inside this `mv` — no sleeping: it announces itself on one
        # FIFO and waits for a line on the other, which the test writes only after
        # signalling. The test's own writer on the second FIFO is what makes the stub's
        # read return instead of hitting end-of-file.
        ready = os.path.join(self.dir, "ready.fifo")
        gate = os.path.join(self.dir, "gate.fifo")
        blocked = os.path.join(self.dir, "blocked.once")
        os.mkfifo(ready)
        os.mkfifo(gate)
        fail_dir = self.mkdir("signal-mv")
        real_mv = shutil.which("mv")
        self.stub("mv", f"""#!/bin/sh
if [ ! -e "{blocked}" ]; then
  case "$2" in
    *.previous)
      : > "{blocked}"
      printf 'blocked\\n' > "{ready}"
      read -r go < "{gate}" || true ;;
  esac
fi
exec '{real_mv}' "$@"
""", directory=fail_dir)

        ready_fd = os.open(ready, os.O_RDONLY | os.O_NONBLOCK)
        self.addCleanup(os.close, ready_fd)
        # Held open for the whole test: it is the stub's writer, so `read` never sees EOF.
        gate_fd = os.open(gate, os.O_RDWR)
        self.addCleanup(os.close, gate_fd)
        env = self.env(PATH=fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH)
        proc = subprocess.Popen([BASH, INSTALL, "--prefix", self.prefix], env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                start_new_session=True)
        try:
            readable, _, _ = select.select([ready_fd], [], [], 60)
            self.assertTrue(readable, "the install never reached the commit")
            self.assertEqual(os.read(ready_fd, 64), b"blocked\n")
            os.kill(proc.pid, signal.SIGTERM)
            os.write(gate_fd, b"go\n")
            out, err = proc.communicate(timeout=60)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.communicate()
        # The interrupt ends the script after the rollback instead of letting it resume
        # the half-swapped transaction.
        self.assertEqual(proc.returncode, 130, (out, err))
        for name, path in paths.items():
            self.assertEqual(self.read(path), before[name], name)
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "bin"))
                          if name.startswith(".p1")], [])
        self.assertEqual([name for name in os.listdir(os.path.join(self.prefix, "share"))
                          if name.startswith(".p1")], [])

    def test_a_failed_rollback_is_reported_and_reuses_one_backup_slot(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)

        fail_dir = self.mkdir("fail-revert")
        real_mv = shutil.which("mv")
        self.stub("mv", f"""#!/bin/sh
case "$2" in
  *.previous) exec '{real_mv}' "$@" ;;
esac
case "$1" in
  *.previous) exit 74 ;;
esac
case " $* " in
  *"/share/p1 "*) exit 71 ;;
  *"/bin/p1 "*) exit 71 ;;
esac
exec '{real_mv}' "$@"
""", directory=fail_dir)
        env = {"PATH": fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH}
        leftovers = {
            "bin": [".p1-update.previous", ".p1.previous"],
            "share": [".p1.previous"],
        }

        def hidden(where: str) -> list:
            return sorted(name for name in os.listdir(os.path.join(self.prefix, where))
                          if name.startswith(".p1"))

        for attempt in ("first", "second"):
            self.publish(marker="two", binary=NEW_RELEASE)
            done = self.run_install("--prefix", self.prefix, **env)
            self.assertNotEqual(done.returncode, 0, done.stdout)
            self.assertIn("could not commit the new share data and binary", done.stderr)
            # A restore that fails is reported with the paths it could not put back.
            self.assertIn("could not be restored", done.stderr)
            for where, names in leftovers.items():
                for name in names:
                    path = os.path.join(self.prefix, where, name)
                    self.assertIn(path, done.stderr, attempt)
                    self.assertTrue(os.path.exists(path), (attempt, path))
                # One fixed slot per prefix: a second failure does not accumulate copies.
                self.assertEqual(hidden(where), names, attempt)
            self.assertFalse(os.path.exists(os.path.join(self.prefix, "bin", "p1")), attempt)
            self.assertFalse(os.path.exists(os.path.join(self.prefix, "share", "p1")), attempt)

            # The stale slot is replaced by the next install, which succeeds from scratch.
            self.publish()
            healthy = self.run_install("--prefix", self.prefix)
            self.assertEqual(healthy.returncode, 0, healthy.stderr)
            self.assert_installed(self.prefix)
            self.assertEqual(hidden("bin"), [], attempt)
            self.assertEqual(hidden("share"), [], attempt)

    def test_a_failed_rollback_backup_survives_another_failed_install_until_success(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        backup = os.path.join(self.prefix, "bin", ".p1.previous")

        fail_dir = self.mkdir("preserve-backup")
        real_mv = shutil.which("mv")
        self.stub("mv", f"""#!/bin/sh
case \"$1\" in
  *.previous) exit 74 ;;
esac
case \" $* \" in
  *"/share/p1 "*) exit 71 ;;
esac
exec '{real_mv}' \"$@\"
""", directory=fail_dir)
        self.publish(marker="two", binary=NEW_RELEASE)
        env = {"PATH": fail_dir + ":" + self.stub_dir + ":" + SYSTEM_PATH}
        failed = self.run_install("--prefix", self.prefix, **env)
        self.assertNotEqual(failed.returncode, 0, failed.stdout)
        self.assertIn("could not be restored", failed.stderr)
        self.assertTrue(os.path.isfile(backup))
        backup_bytes = self.read(backup)

        # A second failed commit must not delete that only manual-recovery copy before it
        # starts. The final successful install, by contrast, may discard it.
        failed_again = self.run_install("--prefix", self.prefix, **env)
        self.assertNotEqual(failed_again.returncode, 0, failed_again.stdout)
        self.assertEqual(self.read(backup), backup_bytes)
        self.publish()
        healthy = self.run_install("--prefix", self.prefix)
        self.assertEqual(healthy.returncode, 0, healthy.stderr)
        self.assertFalse(os.path.exists(backup))

    def test_the_update_wrapper_reinstalls_without_a_checkout(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        # Publish a new release; the wrapper must pick it up through the installed copy.
        self.publish(marker="two", binary=FAKE_P1.replace("fake p1", "fake p1 v2")
                     .replace("deadbeef0000", "cafebabe0000"))
        done = subprocess.run([os.path.join(self.prefix, "bin", "p1-update")],
                              env=self.env(), capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("p1 0.0.1 (cafebabe0000 2026-09-24)", done.stdout)
        self.assertEqual(self.read(os.path.join(self.prefix, "share", "p1",
                                                "environments", "marker.txt")),
                         "environments two\n")
        self.assertIn("fake p1 v2", self.read(os.path.join(self.prefix, "bin", "p1")))
        self.assertEqual(len(self.gh_downloads()), 8)

    def test_same_release_is_a_noop_and_force_reinstalls(self) -> None:
        first = self.run_install("--from-release", "v9.9.9", "--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        gh_count = len(self.gh_downloads())
        mtime = os.stat(os.path.join(self.prefix, "bin", "p1")).st_mtime_ns

        same = self.run_install("--from-release", "v9.9.9", "--prefix", self.prefix)
        self.assertEqual(same.returncode, 0, same.stderr)
        self.assertIn("already installed", same.stdout)
        self.assertEqual(len(self.gh_downloads()), gh_count)
        self.assertEqual(os.stat(os.path.join(self.prefix, "bin", "p1")).st_mtime_ns, mtime)

        with open(os.path.join(self.prefix, "share", "p1", "environments", "marker.txt"),
                  "w", encoding="utf-8") as handle:
            handle.write("changed before forced reinstall\n")
        forced = self.run_install("--from-release", "v9.9.9", "--prefix", self.prefix, "--force")
        self.assertEqual(forced.returncode, 0, forced.stderr)
        self.assertEqual(len(self.gh_downloads()), gh_count + 4)
        self.assertEqual(self.read(os.path.join(self.prefix, "share", "p1",
                                                "environments", "marker.txt")),
                         "environments one\n")

    def test_latest_of_the_installed_commit_is_a_noop_and_force_reinstalls(self) -> None:
        # `p1 --version` prints `p1 <version> (<sha> <date>)`: the installed commit must be
        # recognized from that line, so a second --latest downloads nothing at all.
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(len(self.gh_downloads()), 4, self.log(self.gh_log))
        binary = os.path.join(self.prefix, "bin", "p1")
        mtime = os.stat(binary).st_mtime_ns

        same = self.run_install("--prefix", self.prefix)
        self.assertEqual(same.returncode, 0, same.stderr)
        self.assertIn("already installed", same.stdout)
        self.assertEqual(len(self.gh_downloads()), 4, self.log(self.gh_log))
        self.assertEqual(os.stat(binary).st_mtime_ns, mtime)
        self.assertEqual(self.read(binary), FAKE_P1)

        forced = self.run_install("--prefix", self.prefix, "--force")
        self.assertEqual(forced.returncode, 0, forced.stderr)
        self.assertEqual(len(self.gh_downloads()), 8)
        self.assert_installed(self.prefix)

    def test_latest_noop_without_gh_uses_the_releases_redirect(self) -> None:
        first = self.run_install("--prefix", self.prefix, **self.no_gh())
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(len(self.curl_downloads()), 4, self.log(self.curl_log))
        mtime = os.stat(os.path.join(self.prefix, "bin", "p1")).st_mtime_ns

        same = self.run_install("--prefix", self.prefix, **self.no_gh())
        self.assertEqual(same.returncode, 0, same.stderr)
        self.assertIn("already installed", same.stdout)
        # One releases/latest probe, and no asset fetched.
        self.assertEqual(len(self.curl_downloads()), 4, self.log(self.curl_log))
        self.assertIn("/releases/latest", self.log(self.curl_log))
        self.assertEqual(os.stat(os.path.join(self.prefix, "bin", "p1")).st_mtime_ns, mtime)

    def test_from_release_of_the_installed_commit_is_a_noop(self) -> None:
        first = self.run_install("--prefix", self.prefix)
        self.assertEqual(first.returncode, 0, first.stderr)
        calls = len(self.log(self.gh_log).splitlines())
        binary = os.path.join(self.prefix, "bin", "p1")
        mtime = os.stat(binary).st_mtime_ns

        # main-<sha> names the commit the installed binary prints; no lookup, no download.
        same = self.run_install("--from-release", "main-deadbeef0000", "--prefix", self.prefix)
        self.assertEqual(same.returncode, 0, same.stderr)
        self.assertIn("already installed", same.stdout)
        self.assertEqual(len(self.log(self.gh_log).splitlines()), calls)
        self.assertEqual(os.stat(binary).st_mtime_ns, mtime)

    def test_explicit_release_allows_a_downgrade(self) -> None:
        self.run_install("--from-release", "v2.0.0", "--prefix", self.prefix)
        done = self.run_install("--from-release", "v1.0.0", "--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.read(os.path.join(self.prefix, "share", "p1", ".p1-release")),
                         "v1.0.0\n")

    def test_the_installed_installer_is_the_one_the_wrapper_runs(self) -> None:
        self.run_install("--prefix", self.prefix)
        wrapper = self.read(os.path.join(self.prefix, "bin", "p1-update"))
        self.assertIn(f'exec "{self.prefix}/share/p1/install.sh" --latest --prefix "{self.prefix}"',
                      wrapper)

    def test_update_sh_forwards_to_install_sh(self) -> None:
        done = subprocess.run([BASH, UPDATE, "--prefix", self.prefix], env=self.env(),
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)

    def test_piped_to_bash_installs_the_binary_but_not_the_self_update(self) -> None:
        # `curl ... | bash` has no script file to copy: the install works, and the
        # wrapper is left out rather than pointed at a path that does not exist.
        env = self.env(**self.no_gh())
        done = subprocess.run([BASH, "-s", "--", "--prefix", self.prefix],
                              input=self.read(INSTALL), env=env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("no script file to copy", done.stderr)
        binary = os.path.join(self.prefix, "bin", "p1")
        self.assertEqual(self.read(binary), FAKE_P1)
        self.assertTrue(os.path.isdir(os.path.join(self.prefix, "share", "p1", "environments")))
        self.assertFalse(os.path.exists(os.path.join(self.prefix, "share", "p1", "install.sh")))
        self.assertFalse(os.path.exists(os.path.join(self.prefix, "bin", "p1-update")))

    # --- --local -----------------------------------------------------------

    def test_local_uses_the_default_ssd_target_and_fixed_build_limits(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs()
        done = self.run_install("--local", "--prefix", self.prefix, CARGO_BUILD_JOBS="1")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        target = os.path.join(self.home, ".cache", "cargo-target", "p1-release")
        cargo = self.log(self.cargo_log)
        self.assertIn(f"target={target}\n", cargo)
        self.assertIn("jobs=2\n", cargo)
        self.assertIn(f"wrapper={os.path.join(SCRIPTS, 'rustc-serial')}\n", cargo)
        self.assertIn("argv=build --release --locked -p p1-host", cargo)
        # Share data come from this checkout.
        self.assertTrue(os.path.isdir(os.path.join(self.prefix, "share", "p1", "environments",
                                                   "claude")))

    def test_local_uses_cargo_target_dir_override(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs()
        target = self.mkdir(os.path.join("home", ".cache", "cargo-target", "task-local"))
        done = self.run_install("--local", "--prefix", self.prefix, CARGO_TARGET_DIR=target)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)
        self.assertIn(f"target={target}\n", self.log(self.cargo_log))

    def test_local_refuses_a_relative_cargo_target_dir(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs()
        done = self.run_install("--local", "--prefix", self.prefix, CARGO_TARGET_DIR="task-local")
        self.assert_local_refused(done, "CARGO_TARGET_DIR must be an absolute path: task-local")
        self.assertFalse(os.path.exists(os.path.join(os.path.dirname(SCRIPTS), "task-local")))

    def test_local_refuses_a_target_outside_the_cache_root(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs()
        target = self.mkdir("outside-target")
        done = self.run_install("--local", "--prefix", self.prefix, CARGO_TARGET_DIR=target)
        self.assert_local_refused(done, "must resolve below")

    def test_local_refuses_more_than_two_build_jobs(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs()
        done = self.run_install("--local", "--prefix", self.prefix, CARGO_BUILD_JOBS="3")
        self.assert_local_refused(done, "CARGO_BUILD_JOBS must not exceed 2: 3")

    def test_local_refuses_a_target_below_the_free_space_threshold(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs(free=12884901887)
        target = self.mkdir(os.path.join("home", ".cache", "cargo-target", "task-local"))
        done = self.run_install("--local", "--prefix", self.prefix, CARGO_TARGET_DIR=target)
        self.assert_local_refused(done, "below the 12884901888-byte admission threshold")

    def test_local_admits_exactly_the_free_space_threshold(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs(free=12884901888)
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_installed(self.prefix)

    def test_local_refuses_a_tmpfs_target(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs(filesystem="tmpfs")
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assert_local_refused(done, "not on an ext4 filesystem")
        self.assertIn("found tmpfs", done.stderr)

    def test_local_refuses_an_ext3_target(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs(filesystem="ext3")
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assert_local_refused(done, "not on an ext4 filesystem")
        self.assertIn("found ext3", done.stderr)

    def test_local_refuses_a_failing_findmnt(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub("findmnt", "#!/bin/sh\nexit 7\n", directory=self.stub_dir)
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assert_local_refused(done, "cannot inspect target filesystem")

    def test_local_refuses_malformed_findmnt_output(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub("findmnt", "#!/bin/sh\nexit 0\n", directory=self.stub_dir)
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assert_local_refused(done, "not on an ext4 filesystem")
        self.assertIn("found unparseable", done.stderr)

    def test_local_refuses_a_failing_df(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub("findmnt", "#!/bin/sh\nprintf '%s\\n' ext4\n", directory=self.stub_dir)
        self.stub("df", "#!/bin/sh\nexit 7\n", directory=self.stub_dir)
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assert_local_refused(done, "cannot inspect target free space")

    def test_local_refuses_malformed_df_output(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub("findmnt", "#!/bin/sh\nprintf '%s\\n' ext4\n", directory=self.stub_dir)
        self.stub("df", "#!/bin/sh\nprintf '%s\\n' nonsense\n", directory=self.stub_dir)
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assert_local_refused(done, "cannot read target free space")

    def test_local_refuses_a_failing_awk(self) -> None:
        self.stub("cargo", CARGO_STUB)
        self.stub_local_fs()
        self.stub("awk", "#!/bin/sh\nexit 9\n", directory=self.stub_dir)
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assert_local_refused(done, "cannot parse target free space")

    def test_local_reports_a_missing_build_artifact(self) -> None:
        stub = self.stub("cargo", "#!/bin/sh\nexit 0\n")
        self.stub_local_fs()
        self.assertTrue(os.path.isfile(stub))
        done = self.run_install("--local", "--prefix", self.prefix)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("cargo did not produce", done.stderr)
        self.assert_nothing_installed(self.prefix)


if __name__ == "__main__":
    unittest.main()
