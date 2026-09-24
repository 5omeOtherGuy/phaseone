#!/usr/bin/env python3
"""Unit tests for scripts/local-cargo-config.sh — stdlib unittest, no Cargo.

    python3 scripts/test_local_cargo_config.py

Each test runs the Bash script with a synthetic PATH containing deterministic
findmnt and git stubs.  Checkouts and stubs live only in temporary directories;
no test creates, removes, or depends on state below /mnt/build.
"""
from __future__ import annotations

import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "local-cargo-config.sh")
TARGET_ROOT = "/mnt/build/cargo-target"

# The script deliberately requires the exact /mnt/build ext4 entry.  An autofs
# entry by itself simulates storage that is not available yet.
FINDMNT_STUB = r"""#!/bin/sh
case "${FIXTURE:-ext4}" in
  ext4)
    printf '%s\n' '/mnt/build autofs' '/mnt/build ext4'
    ;;
  autofs)
    printf '%s\n' '/mnt/build autofs'
    ;;
  nested)
    path=
    while [ $# -gt 0 ]; do
      case $1 in
        -T)
          path=$2
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    if [ "$path" = '/mnt/build/.' ]; then
      printf '%s\n' '/mnt/build ext4'
    else
      printf '%s\n' '/mnt/build/cargo-target foreignfs'
    fi
    ;;
  *)
    printf 'synthetic findmnt failure: %s\n' "$FIXTURE" >&2
    exit 1
    ;;
esac
"""

# Porcelain output is controlled entirely by the test environment; the real git
# executable and any worktrees on the test machine are never consulted.
GIT_STUB = r"""#!/bin/sh
printf 'worktree %s\n' "${GIT_MAIN_WORKTREE:?}"
printf '\n'
"""


def write_executable(path: str, text: str) -> None:
    """Write one synthetic executable in a test-owned temporary directory."""
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    os.chmod(path, 0o755)


def target_for(checkout: str) -> str:
    """Return the target the script derives from a canonical checkout path.

    The script composes "<target-root>/<basename>-<hash>" and canonicalises the
    result with `realpath -m`, so canonicalise the target root here too rather
    than trusting the literal constant.
    """
    canonical = os.path.realpath(checkout)
    digest = hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:12]
    return os.path.join(
        os.path.realpath(TARGET_ROOT), f"{os.path.basename(canonical)}-{digest}"
    )


@unittest.skipUnless(shutil.which("bash"), "bash is required")
class LocalCargoConfigTest(unittest.TestCase):
    """Exercise storage validation and generated TOML without live HDD state."""

    def setUp(self) -> None:
        self.root = tempfile.mkdtemp(prefix="local-cargo-config-test-")
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.bin = os.path.join(self.root, "bin")
        os.mkdir(self.bin)
        write_executable(os.path.join(self.bin, "findmnt"), FINDMNT_STUB)
        write_executable(os.path.join(self.bin, "git"), GIT_STUB)
        self.checkout, self.main_worktree = self.make_checkout("checkout")

    def make_checkout(
        self,
        relative: str,
        *,
        local_wrapper: bool = True,
        main_worktree: str | None = None,
    ) -> tuple[str, str]:
        """Make a checkout and its synthetic main worktree under the temp root."""
        checkout = os.path.realpath(os.path.join(self.root, relative))
        os.makedirs(os.path.join(checkout, "scripts"), exist_ok=True)
        if main_worktree is None:
            main_worktree = checkout
        else:
            main_worktree = os.path.realpath(main_worktree)
            os.makedirs(os.path.join(main_worktree, "scripts"), exist_ok=True)
            write_executable(
                os.path.join(main_worktree, "scripts", "rustc-serial"),
                "#!/bin/sh\nexit 0\n",
            )
        if local_wrapper:
            write_executable(
                os.path.join(checkout, "scripts", "rustc-serial"),
                "#!/bin/sh\nexit 0\n",
            )
        return checkout, main_worktree

    def environment(
        self,
        main_worktree: str,
        fixture: str = "ext4",
    ) -> dict[str, str]:
        """Build an environment with only the synthetic findmnt/git commands first."""
        env = os.environ.copy()
        for name in (
            "BASH_ENV",
            "CARGO_TARGET_DIR",
            "ENV",
            "FIXTURE",
            "GIT_MAIN_WORKTREE",
        ):
            env.pop(name, None)
        env["PATH"] = self.bin + os.pathsep + env.get("PATH", "")
        env["FIXTURE"] = fixture
        env["GIT_MAIN_WORKTREE"] = main_worktree
        return env

    def run_script(
        self,
        *args: str,
        checkout: str | None = None,
        fixture: str = "ext4",
        target_override: str | None = None,
        main_worktree: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Invoke the real script under test with bounded subprocess execution."""
        selected = checkout or self.checkout
        reported_main = main_worktree or self.main_worktree
        env = self.environment(reported_main, fixture=fixture)
        if target_override is not None:
            env["CARGO_TARGET_DIR"] = target_override
        return subprocess.run(
            ["bash", SCRIPT, *args, selected],
            cwd=self.root,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
            check=False,
        )

    def assert_config_absent(self, checkout: str) -> None:
        """A rejected or dry-run invocation must not write Cargo configuration."""
        self.assertFalse(
            os.path.exists(os.path.join(checkout, ".cargo", "config.toml")),
            "the script unexpectedly wrote config.toml",
        )

    def test_dry_run_default_is_valid_isolated_toml(self) -> None:
        result = self.run_script("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        config = tomllib.loads(result.stdout)
        expected_target = target_for(self.checkout)
        self.assertEqual(config["build"]["target-dir"], expected_target)
        self.assertRegex(
            expected_target,
            rf"^{re.escape(os.path.realpath(TARGET_ROOT))}/"
            rf"{re.escape(os.path.basename(self.checkout))}-[0-9a-f]{{12}}$",
        )
        self.assertEqual(config["build"]["jobs"], 2)
        self.assertIs(config["build"]["incremental"], False)
        self.assertEqual(
            config["build"]["rustc-wrapper"],
            os.path.join(self.checkout, "scripts", "rustc-serial"),
        )
        self.assertTrue(
            config["build"]["rustc-wrapper"].endswith("scripts/rustc-serial")
        )
        self.assert_config_absent(self.checkout)

    def test_dry_run_contains_one_jobs_setting(self) -> None:
        result = self.run_script("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count("jobs = 2"), 1)

    def test_dry_run_rejects_config_symlink_without_touching_victim(self) -> None:
        config_dir = os.path.join(self.checkout, ".cargo")
        victim = os.path.join(self.checkout, "Cargo.toml")
        config = os.path.join(config_dir, "config.toml")
        victim_contents = b"known Cargo manifest contents\n"
        os.mkdir(config_dir)
        with open(victim, "wb") as handle:
            handle.write(victim_contents)
        os.symlink("../Cargo.toml", config)

        result = self.run_script("--dry-run")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo config must not be a symlink", result.stderr)
        with open(victim, "rb") as handle:
            self.assertEqual(handle.read(), victim_contents)

    def test_dry_run_rejects_config_directory_symlink_without_touching_file(self) -> None:
        real_config_dir = os.path.join(self.checkout, "real-cargo-config")
        config_dir_link = os.path.join(self.checkout, ".cargo")
        victim = os.path.join(real_config_dir, "config.toml")
        victim_contents = b"alternate config contents\n"
        os.mkdir(real_config_dir)
        with open(victim, "wb") as handle:
            handle.write(victim_contents)
        os.symlink("real-cargo-config", config_dir_link)

        result = self.run_script("--dry-run")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo config parent must not be a symlink", result.stderr)
        with open(victim, "rb") as handle:
            self.assertEqual(handle.read(), victim_contents)

    def test_dry_run_rejects_dangling_config_symlink(self) -> None:
        config_dir = os.path.join(self.checkout, ".cargo")
        config = os.path.join(config_dir, "config.toml")
        missing_target = os.path.join(config_dir, "missing.toml")
        os.mkdir(config_dir)
        os.symlink("missing.toml", config)

        result = self.run_script("--dry-run")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo config must not be a symlink", result.stderr)
        self.assertEqual(os.readlink(config), "missing.toml")
        self.assertFalse(os.path.exists(missing_target))

    def test_regular_config_is_accepted_and_dry_runs_are_identical(self) -> None:
        config_dir = os.path.join(self.checkout, ".cargo")
        config = os.path.join(config_dir, "config.toml")
        config_contents = b"existing config contents\n"
        os.mkdir(config_dir)
        with open(config, "wb") as handle:
            handle.write(config_contents)

        first = self.run_script("--dry-run")
        second = self.run_script("--dry-run")

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(first.stdout, second.stdout)
        with open(config, "rb") as handle:
            self.assertEqual(handle.read(), config_contents)

    def test_same_basename_checkouts_get_path_isolated_targets(self) -> None:
        first, _ = self.make_checkout("first/same-name")
        second, _ = self.make_checkout("second/same-name")

        first_result = self.run_script("--dry-run", checkout=first)
        second_result = self.run_script("--dry-run", checkout=second)
        self.assertEqual(first_result.returncode, 0, first_result.stderr)
        self.assertEqual(second_result.returncode, 0, second_result.stderr)
        first_target = tomllib.loads(first_result.stdout)["build"]["target-dir"]
        second_target = tomllib.loads(second_result.stdout)["build"]["target-dir"]
        self.assertEqual(first_target, target_for(first))
        self.assertEqual(second_target, target_for(second))
        self.assertNotEqual(first_target, second_target)
        self.assert_config_absent(first)
        self.assert_config_absent(second)

    def test_explicit_target_below_build_root_is_accepted(self) -> None:
        target = f"{TARGET_ROOT}/explicit-build"
        result = self.run_script(
            "--dry-run", target_override=target
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        config = tomllib.loads(result.stdout)
        self.assertEqual(config["build"]["target-dir"], target)
        self.assert_config_absent(self.checkout)

    def test_invalid_target_overrides_are_rejected(self) -> None:
        invalid_targets = (
            ("empty", "", "must not be empty"),
            ("relative", "relative-target", "must be absolute"),
            ("filesystem root", "/", "resolves outside"),
            ("target root", TARGET_ROOT, "not /mnt/build/cargo-target itself"),
            ("parent escape", "../escape", "must be absolute"),
            ("absolute escape", f"{TARGET_ROOT}/../escape", "resolves outside"),
            (
                "outside root",
                "/var/tmp/local-cargo-config-outside-root",
                "resolves outside",
            ),
        )
        for label, target, message in invalid_targets:
            with self.subTest(override=label, value=target):
                result = self.run_script(
                    "--dry-run", target_override=target
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("local-cargo-config:", result.stderr)
                self.assertIn(message, result.stderr)
                self.assertTrue(result.stderr.strip())
                self.assert_config_absent(self.checkout)

    def test_autofs_without_ext4_is_rejected(self) -> None:
        result = self.run_script("--dry-run", fixture="autofs")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required /mnt/build ext4", result.stderr)
        self.assert_config_absent(self.checkout)

    def test_nested_foreign_mount_at_target_ancestor_is_rejected(self) -> None:
        result = self.run_script("--dry-run", fixture="nested")
        self.assertNotEqual(result.returncode, 0)
        # The first probe (for /mnt/build/.) must succeed, so the rejection has to
        # come from the ancestor probe rather than the build-root probe.
        self.assertNotIn("/mnt/build/. is not on", result.stderr)
        self.assertIn(
            "is not on the required /mnt/build ext4 filesystem", result.stderr
        )
        self.assert_config_absent(self.checkout)

    def test_quote_and_backslash_checkout_yields_exact_toml_paths(self) -> None:
        checkout, _ = self.make_checkout('escaped/quote"\\checkout')
        result = self.run_script("--dry-run", checkout=checkout)
        self.assertEqual(result.returncode, 0, result.stderr)
        config = tomllib.loads(result.stdout)
        self.assertEqual(config["build"]["target-dir"], target_for(checkout))
        self.assertEqual(
            config["build"]["rustc-wrapper"],
            os.path.join(checkout, "scripts", "rustc-serial"),
        )
        self.assert_config_absent(checkout)

    def test_too_many_arguments_exit_two(self) -> None:
        result = self.run_script("--dry-run", "extra-argument")
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("Usage:", result.stderr)
        self.assert_config_absent(self.checkout)

    def test_checkout_local_wrapper_precedes_main_worktree_wrapper(self) -> None:
        main, _ = self.make_checkout("main/checkout")
        checkout, _ = self.make_checkout(
            "linked/checkout", main_worktree=main
        )
        result = self.run_script("--dry-run", checkout=checkout)
        self.assertEqual(result.returncode, 0, result.stderr)
        config = tomllib.loads(result.stdout)
        self.assertEqual(
            config["build"]["rustc-wrapper"],
            os.path.join(checkout, "scripts", "rustc-serial"),
        )
        self.assertNotEqual(
            config["build"]["rustc-wrapper"],
            os.path.join(main, "scripts", "rustc-serial"),
        )
        self.assert_config_absent(checkout)

    def test_main_worktree_wrapper_used_when_local_wrapper_absent(self) -> None:
        main, _ = self.make_checkout("main/checkout")
        checkout, _ = self.make_checkout("linked/checkout", local_wrapper=False)
        result = self.run_script(
            "--dry-run", checkout=checkout, main_worktree=main
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        config = tomllib.loads(result.stdout)
        self.assertEqual(
            config["build"]["rustc-wrapper"],
            os.path.join(main, "scripts", "rustc-serial"),
        )
        self.assert_config_absent(checkout)

    def test_missing_executable_wrapper_is_rejected(self) -> None:
        main, _ = self.make_checkout("main/checkout", local_wrapper=False)
        checkout, _ = self.make_checkout("linked/checkout", local_wrapper=False)
        result = self.run_script(
            "--dry-run", checkout=checkout, main_worktree=main
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rustc-serial wrapper is not executable", result.stderr)
        self.assert_config_absent(checkout)

    def test_actual_mode_safely_rejects_absent_build_storage(self) -> None:
        # Actual mode is intentionally not allowed to proceed far enough to call
        # mkdir.  On both mounted and unmounted test machines, the stubbed
        # autofs-only result models an absent build root and fails validation
        # before the real /mnt/build tree can be inspected or modified.
        result = self.run_script(fixture="autofs")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required /mnt/build ext4", result.stderr)
        self.assert_config_absent(self.checkout)


if __name__ == "__main__":
    unittest.main()
