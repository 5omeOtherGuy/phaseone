#!/usr/bin/env python3
"""Pre-push selection regressions: stdlib, synthetic tools, no Rust builds."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent
SELECTOR = ROOT / "scripts/pre_push_packages.py"


def metadata(root, manifests):
    packages = [
        {"id": name, "name": name, "manifest_path": str(root / path)}
        for name, path in manifests.items()
    ]
    return {"workspace_root": str(root), "packages": packages,
            "workspace_members": list(manifests)}


def select(data, *changed):
    result = subprocess.run(
        [sys.executable, str(SELECTOR), *changed], input=json.dumps(data),
        capture_output=True, text=True, check=True,
    )
    return set(result.stdout.splitlines())


class PrePushTests(unittest.TestCase):
    def test_real_shipped_directories_select_their_readers(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
        manifests = {}
        for pattern in workspace["members"]:
            for directory in ROOT.glob(pattern):
                manifest = directory / "Cargo.toml"
                if manifest.is_file():
                    name = tomllib.loads(manifest.read_text())["package"]["name"]
                    manifests[name] = manifest.relative_to(ROOT)
        data = metadata(ROOT, manifests)
        for directory, expected in {
            # The provider crates and p1-usage carry inline fixtures since #844, and
            # p1-module-runtime, p1-workflow and the p1-module-tests behavior suites write
            # temporary catalogs since #843: none of them reads a shipped catalog file any more.
            "routes": {"p1-host"},
            "accounts": {"p1-host"},
            "environments": {"p1-host", "p1-assembly", "p1-live", "p1-module-tests"},
            "profiles": {"p1-host", "p1-assembly", "p1-live", "p1-model-profile"},
            # The subscription connectors' routes (#852): the host's own pins and the
            # connectors' test crate read them by path.
            "connectors": {"p1-host", "p1-connector-tests"},
            # The owner pack (#853): the prompt sweep and the distribution's own pins
            # read its environments, accounts and scripts by path.
            "packs": {"p1-host", "p1-assembly", "p1-workflow"},
        }.items():
            with self.subTest(directory=directory):
                selected = select(data, f"{directory}/fake.toml")
                self.assertTrue(expected <= selected, expected - selected)
                self.assertNotIn("p1-workspace", selected)
        # The component suites write their routes into a temporary catalog (#843): a route
        # change must not select them.
        self.assertNotIn("p1-module-tests", select(data, "routes/fake.toml"))
        self.assertEqual(select(data, "crates/p1-usage/src/probe.rs"), {"p1-usage"})
        self.assertEqual(select(data, "Cargo.toml", "Cargo.lock"), set())
        self.assertEqual(select(data, "scripts/pre-push.sh", "AGENTS.md"), set())

    def test_new_readers_and_data_directories_need_no_inventory_update(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sources = {
                "embedded": ('tests/read.rs', 'include_bytes!("../../../routes/new.bin");'),
                "built": ('build.rs', 'Path::new(&root).join("../../accounts");'),
                "joined": ('src/lib.rs', 'Path::new(env!("CARGO_MANIFEST_DIR"))\n'
                           '.join("../..").join(\n "environments"\n);'),
                "helper": ('tests/read.rs', 'repo("profiles/new.toml");'),
                "rooted": ('tests/read.rs', 'let dir = repo_root().join("routes");\n'
                           'checkout.join(&format!("profiles/{id}.toml"));'),
                "temporary": ('tests/fixture.rs', 'let catalog = tempfile::tempdir().unwrap();\n'
                              'let route = catalog.path().join(format!("routes/{id}.toml"));\n'
                              'std::fs::create_dir_all(scratch.join("profiles")).unwrap();\n'
                              'let dirs = vec![config.join("environments")];'),
                "new-data": ('src/lib.rs', 'include_str!(concat!(\n'
                             'env!("CARGO_MANIFEST_DIR"), "/../../datasets/new.json"));'),
                "unrelated": ('src/lib.rs', 'let label = "routes";\n'
                              'let user = ".config/p1/environments/default.toml";'),
            }
            manifests = {}
            for name, (source, text) in sources.items():
                manifests[name] = f"crates/{name}/Cargo.toml"
                path = root / f"crates/{name}" / source
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            data = metadata(root, manifests)
            self.assertEqual(select(data, "routes/deleted.bin", "accounts/deleted.toml",
                                    "environments/deleted.toml", "profiles/deleted.toml",
                                    "datasets/deleted.json"),
                             set(sources) - {"unrelated", "temporary"})
            data["packages"].append({"id": "dependency", "name": "dependency",
                                     "manifest_path": str(root / "Cargo.toml")})
            data["packages"].append({"id": "nested", "name": "nested",
                                     "manifest_path": str(root / "crates/helper/logic/Cargo.toml")})
            data["workspace_members"].append("nested")
            self.assertEqual(select(data, "crates/helper/logic/src/lib.rs"), {"nested"})
            self.assertEqual(select(data, "modules/demo/src/lib.rs"), {"p1-module-tests"})

    def test_pre_push_runs_selected_packages_without_building_the_workspace(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for directory in ("scripts", "bin", ".cargo", "crates/reader/tests"):
                (root / directory).mkdir(parents=True)
            shutil.copy2(ROOT / "scripts/pre-push.sh", root / "scripts/pre-push.sh")
            shutil.copy2(SELECTOR, root / "scripts/pre_push_packages.py")
            (root / ".cargo/config.toml").write_text('rustc-wrapper = "rustc-serial"\n')
            (root / "crates/reader/tests/read.rs").write_text('repo("routes/new.toml");')
            (root / "metadata.json").write_text(json.dumps(
                metadata(root, {"reader": "crates/reader/Cargo.toml"})))
            stubs = {
                "bin/git": '#!/bin/sh\ncase "$1" in\n'
                           'rev-parse) [ "$2" != --show-toplevel ] || pwd;;\n'
                           'diff) echo routes/new.toml;;\nesac\nexit 0\n',
                "bin/cargo": '#!/bin/sh\nprintf "%s\\n" "$*" >> calls.log\n'
                             'if [ "$1" = metadata ]; then cat metadata.json; fi\n',
                "scripts/build-admission.sh": '#!/bin/sh\necho admission >> calls.log\nexit 0\n',
                "scripts/build-modules.sh": '#!/bin/sh\nexit 0\n',
            }
            for name, text in stubs.items():
                path = root / name
                path.write_text(text)
                path.chmod(0o755)
            result = subprocess.run(
                ["bash", "scripts/pre-push.sh"], cwd=root,
                env={**os.environ, "HOME": str(root), "PATH": f'{root / "bin"}:/usr/bin:/bin',
                     "P1_PREPUSH_CLIPPY": "0", "P1_PREPUSH_TESTS": "1"},
                capture_output=True, text=True, check=True,
            )
            calls = (root / "calls.log").read_text().splitlines()
            self.assertIn("test --locked --no-fail-fast -p reader", calls)
            self.assertFalse(any("test --workspace" in call for call in calls))
            self.assertIn("ok    test reader", result.stdout)
            self.assertIn("admission", calls)

            # By default the package tests are the gate's (ADR-0195): the selection is only
            # named, nothing builds, and nothing waits for a build slot.
            (root / "calls.log").write_text("")
            result = subprocess.run(
                ["bash", "scripts/pre-push.sh"], cwd=root,
                env={**os.environ, "HOME": str(root), "PATH": f'{root / "bin"}:/usr/bin:/bin',
                     "P1_PREPUSH_CLIPPY": "0"},
                capture_output=True, text=True, check=True,
            )
            calls = (root / "calls.log").read_text().splitlines()
            self.assertFalse(any(call.startswith("test ") for call in calls), calls)
            self.assertNotIn("admission", calls)
            self.assertIn("the gate runs the full tests of reader", result.stdout)

            # A root manifest change with no selected test package only formats.
            # It must not wait for unrelated builds; optional clippy still must.
            git = root / "bin/git"
            git.write_text(stubs["bin/git"].replace('echo routes/new.toml', 'echo Cargo.toml'))
            for clippy, admission in (("0", False), ("1", True)):
                (root / "calls.log").write_text("")
                subprocess.run(["bash", "scripts/pre-push.sh"], cwd=root,
                    env={**os.environ, "PATH": f'{root / "bin"}:/usr/bin:/bin',
                         "P1_PREPUSH_CLIPPY": clippy}, capture_output=True, text=True, check=True)
                calls = (root / "calls.log").read_text().splitlines()
                self.assertEqual("admission" in calls, admission)


if __name__ == "__main__":
    unittest.main()
