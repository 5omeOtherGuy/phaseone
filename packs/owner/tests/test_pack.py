#!/usr/bin/env python3
"""The owner pack's own check (#853) — stdlib unittest, run against a built p1.

    P1_BIN=<p1 binary> python3 packs/owner/tests/test_pack.py [-q]

`P1_BIN` names the binary; without it, `p1` on PATH. Every call runs with a throwaway home
and configuration directory and the pack selected through `P1_PACKS` (ADR-0201), so no user
file, credential or other pack is read and no provider is reached.

The pins are the owner's: they moved here from p1-assembly's tests, which keep the same rules
for the core `default` environment. `p1 env check` runs over the pack, and every environment
is read back through `p1 env show` as the host resolves it. On a pull request that changes
only `packs/`, CI runs this file against the latest release instead of a build (ADR-0206).
"""
from __future__ import annotations

import json
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

PACK = pathlib.Path(__file__).resolve().parent.parent
ENVIRONMENTS = PACK / "environments"

# Every environment the pack ships; a new one is added here with its pins below.
NAMES = sorted([
    "claude", "claude2", "cline", "cline2", "deepseek", "deepseek-messages",
    "deepseek-review", "deepseek1", "deepseek2", "deepseek3", "finder", "glm", "glm-go",
    "glm-messages", "gpt", "kimi", "librarian", "task", "zen", "zen2", "zen3",
])

# The tool modules the prompt rule covers: a prompt never names one in backticks that its
# environment does not assemble, and names every one it does. `finish` is benched (ADR-0193).
TOOL_MODULES = ["read", "edit", "write", "grep", "shell", "apply_patch", "finish",
                "worker_start", "worker_result", "worker_continue", "worker_cancel"]
WORKER_MODULES = ["worker_start", "worker_result", "worker_continue", "worker_cancel"]

# The researched (window, reserve, threshold) of each environment, from the table in
# docs/design/context-windows.md. ClinePass documents no window: cline and cline2 carry the
# model's, a conservative value rather than a vendor claim.
CONTEXT = {
    **dict.fromkeys(["claude", "claude2", "task"], (1_000_000, 32_000, 500_000)),
    **dict.fromkeys(["gpt", "finder", "librarian"], (272_000, 32_000, 220_000)),
    **dict.fromkeys(["deepseek", "deepseek-messages", "deepseek-review", "deepseek1",
                     "deepseek2", "deepseek3", "cline", "cline2"],
                    (1_000_000, 256_000, 678_464)),
    **dict.fromkeys(["zen", "zen2", "zen3"], (1_048_576, 524_288, 500_000)),
    **dict.fromkeys(["glm", "glm-messages", "glm-go"], (1_000_000, 131_072, 800_000)),
    "kimi": (1_048_576, 131_072, 838_860),
}
# ADR-0136 / ADR-0142 / ADR-0143: old results are shortened only under compaction pressure.
TRIM = {
    **dict.fromkeys(["deepseek", "deepseek-messages", "deepseek-review", "deepseek1",
                     "deepseek2", "deepseek3", "cline", "cline2"], 678_464),
    **dict.fromkeys(["glm", "glm-messages", "glm-go"], 800_000),
    "kimi": 838_860,
}
SKILLS = {"claude", "claude2", "gpt", "glm", "glm-go", "glm-messages", "kimi", "deepseek",
          "deepseek-messages", "deepseek1", "deepseek2", "deepseek3"}


def binary() -> str:
    found = os.environ.get("P1_BIN") or shutil.which("p1")
    if not found:
        raise SystemExit("test_pack: set P1_BIN or put p1 on PATH")
    return found


class P1:
    """One throwaway home for the whole run, and each `env show` read once."""

    def __init__(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="p1-pack-test-")
        base = pathlib.Path(self.tmp.name)
        self.home = base / "home"
        self.workspace = base / "workspace"
        # Instructions and a skill exist, so a prompt carries both blocks when enabled.
        (self.workspace).mkdir(parents=True)
        (self.workspace / "AGENTS.md").write_text("PROJECT\n", encoding="utf-8")
        skill = self.home / ".agents" / "skills" / "example"
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text("---\ndescription: example\n---\nbody\n", encoding="utf-8")
        (self.home / ".agents" / "AGENTS.md").write_text("GLOBAL\n", encoding="utf-8")
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_DATA_HOME": str(self.home / ".local" / "share"),
            "P1_CONFIG_DIR": str(base / "config"),
            "P1_PACKS": str(PACK),
            "LC_ALL": "C",
        }
        self.shown: dict[str, tuple[str, dict]] = {}

    def run(self, *args: str) -> str:
        done = subprocess.run([binary(), *args], env=self.env, cwd=self.workspace,
                              capture_output=True, text=True, timeout=300, check=False)
        if done.returncode != 0:
            raise AssertionError(f"`p1 {' '.join(args)}` exited {done.returncode}:\n"
                                 f"{done.stdout}{done.stderr}")
        return done.stdout

    def show(self, name: str) -> tuple[str, dict]:
        """`p1 env show NAME`: the profile/effort of its `model` line and the resolved JSON."""
        if name not in self.shown:
            out = self.run("env", "show", name)
            model = next(line.split(None, 1)[1] for line in out.splitlines()
                         if line.startswith("model "))
            self.shown[name] = (model, json.loads(out[out.index("{"):]), out)
        return self.shown[name][:2]

    def window(self, name: str) -> int:
        """The effective window of NAME's `context` line: the table's own, else what the route
        serves of the model (ADR-0217)."""
        self.show(name)
        line = next(line for line in self.shown[name][2].splitlines()
                    if line.startswith("context  window "))
        return int(line.split()[2])


P = P1()


def modules(resolved: dict) -> list[str]:
    return [tool["module"] for tool in resolved["tools"]]


def faces(resolved: dict) -> dict[str, str]:
    return {tool["module"]: tool["declaration"]["name"] for tool in resolved["tools"]}


def backticked(text: str) -> set[str]:
    return set(text.split("`")[1::2])


class PackTest(unittest.TestCase):
    def test_the_pack_ships_exactly_these_environments(self) -> None:
        found = sorted(path.name for path in ENVIRONMENTS.iterdir()
                       if (path / "environment.toml").is_file())
        self.assertEqual(found, NAMES)

    def test_env_check_passes_every_environment_of_the_pack(self) -> None:
        reports = json.loads(P.run("env", "check", "--dir", str(PACK), "--json"))
        failed = [report for report in reports if not report["ok"]]
        self.assertEqual(failed, [])
        checked = {report["environment"] for report in reports}
        self.assertLessEqual(set(NAMES), checked)
        for report in reports:
            if report["environment"] in NAMES:
                self.assertTrue(report["file"].startswith(str(ENVIRONMENTS)), report)

    def test_every_environment_carries_its_researched_context_table(self) -> None:
        for name in NAMES:
            with self.subTest(name=name):
                _, resolved = P.show(name)
                context, options = resolved["context"], resolved["options"]
                # The Codex window is the route binding's since ADR-0217, not the table's.
                window = context.get("window_tokens", P.window(name))
                self.assertEqual(window, P.window(name))
                wall = window - context["output_headroom_tokens"]
                self.assertLess(context["summarize_at_tokens"], wall)
                self.assertLess(context["summary_output_tokens"], wall)
                self.assertEqual((window, context["output_headroom_tokens"],
                                  context["summarize_at_tokens"]), CONTEXT[name])
                self.assertEqual(context.get("trim_at_tokens"), TRIM.get(name))
                if resolved["family"] == "deepseek":
                    # dsh's exact formula rather than the #125 rounding (ADR-0136).
                    self.assertEqual(options["max_output_tokens"], 256_000)
                    self.assertEqual(options["reasoning_effort"], "high")
                    self.assertEqual(context["keep_recent_tokens"], 119_040)
                    self.assertEqual(context["summary_output_tokens"], 65_536)
                elif name == "kimi":
                    # ADR-0143: the ADR-0136 formula with an output cap equal to the reserve.
                    self.assertEqual(options["max_output_tokens"],
                                     context["output_headroom_tokens"])
                else:
                    self.assertEqual(context["summarize_at_tokens"] % 10_000, 0)
                if name in ("glm", "glm-messages", "glm-go"):
                    self.assertEqual(context["keep_recent_tokens"], 50_000)
                    self.assertEqual(context["user_verbatim_tokens"], 8_000)
                    self.assertEqual(context["summary_output_tokens"], 20_000)
                    self.assertEqual(options["reasoning_effort"], "high")

    def test_no_environment_sends_max_effort_and_each_states_its_policy(self) -> None:
        # #426: an explicit effort, or reasoning disabled through the Responses option.
        for name in NAMES:
            with self.subTest(name=name):
                model, resolved = P.show(name)
                options = resolved["options"]
                stated = options["reasoning_effort"] is not None or (
                    resolved["route"]["origin"]["route"].startswith("openai-responses/")
                    and options["native"].get("openai-responses.reasoning_enabled") is False)
                self.assertTrue(stated, options)
                self.assertFalse(model.endswith(":max"), model)

    def test_instruction_and_skill_inventory(self) -> None:
        for name in NAMES:
            with self.subTest(name=name):
                _, resolved = P.show(name)
                files = {"claude": ["CLAUDE.md", "AGENTS.md"],
                         "claude2": ["CLAUDE.md", "AGENTS.md"],
                         "gpt": ["AGENTS.override.md", "AGENTS.md"]}.get(name, ["AGENTS.md"])
                roots = (["~/.claude/skills", ".claude/skills", "~/.agents/skills",
                          ".agents/skills"] if name in ("claude", "claude2")
                         else ["~/.agents/skills", ".agents/skills"])
                enabled = name not in ("zen", "zen2", "zen3")
                self.assertEqual(resolved["instructions"]["files"], files)
                self.assertEqual(resolved["instructions"]["enabled"], enabled)
                self.assertEqual(resolved["instructions"]["max_bytes"], 32_768)
                self.assertEqual(resolved["skills"]["roots"], roots)
                self.assertEqual(resolved["skills"]["max_listing_chars"], 8000)
                has_skill = name in SKILLS
                self.assertEqual("skill" in modules(resolved), has_skill)
                if has_skill:
                    self.assertEqual(modules(resolved)[-1], "skill")
                self.assertEqual("<instructions>" in resolved["system_prompt"], enabled)
                self.assertEqual("<skills>" in resolved["system_prompt"], has_skill)

    def test_tool_concurrency_follows_each_vendor(self) -> None:
        # #592: Claude runs every shell call alone; the others take the ADR's default.
        for name, shell_reads in [("claude", False), ("claude2", False), ("gpt", True),
                                  ("glm", True), ("glm-messages", True), ("glm-go", True),
                                  ("deepseek", True), ("kimi", True)]:
            with self.subTest(name=name):
                _, resolved = P.show(name)
                self.assertEqual(resolved["tool_concurrency"],
                                 {"max_parallel": 10, "shell_reads": shell_reads})

    def test_the_review_environment_is_a_leaf(self) -> None:
        # ADR-0124: no worker or workflow tools, so the host adds none.
        _, resolved = P.show("deepseek-review")
        self.assertEqual(modules(resolved), ["read", "grep", "shell", "read_output", "write"])

    def test_the_main_agents_tool_lists(self) -> None:
        delegation = WORKER_MODULES + ["workflow_start", "workflow_status", "workflow_result",
                                       "workflow_cancel"]
        _, claude = P.show("claude")
        self.assertEqual(modules(claude), ["read", "edit", "write", "grep", "shell", "shell_job",
                                           "read_output", "ask_user_question", *delegation,
                                           "todo_write", "skill"])
        self.assertIn("read, edit, write, grep, shell, shell_job, read_output, "
                      "ask_user_question, worker_start", claude["system_prompt"])
        self.assertNotIn("`apply_patch`", claude["system_prompt"])
        _, gpt = P.show("gpt")
        self.assertEqual(modules(gpt), ["shell", "shell_job", "read_output", "ask_user_question",
                                        "apply_patch", *delegation, "skill"])
        self.assertNotIn("`edit`", gpt["system_prompt"])
        self.assertNotIn("`write`", gpt["system_prompt"])

    def test_every_environment_that_lists_shell_lists_read_output(self) -> None:
        # #511: and it resolves to the release's component.
        for name in NAMES:
            with self.subTest(name=name):
                _, resolved = P.show(name)
                if "shell" not in modules(resolved):
                    continue
                tool = next(tool for tool in resolved["tools"] if tool["module"] == "read_output")
                self.assertEqual(tool["identity"]["implementation"], "p1/read-output")

    def test_the_subagent_presets(self) -> None:
        presets = [
            ("finder", "finder/gpt-5.6-terra:low", ["read", "grep"],
             "You are a fast, parallel code search agent."),
            ("librarian", "librarian/gpt-5.6-sol",
             ["read_github", "list_directory_github", "glob_github", "search_github",
              "commit_search", "diff_github", "list_repositories"],
             "You are Librarian, a specialized repository research worker."),
            ("task", "task/claude-opus-5-5:medium",
             ["read", "edit", "write", "grep", "shell", "shell_job", "read_output"],
             "## Task Worker Role"),
        ]
        for name, model, tools, role in presets:
            with self.subTest(name=name):
                shown, resolved = P.show(name)
                self.assertEqual(shown, model)
                self.assertEqual(modules(resolved), tools)
                prompt = resolved["system_prompt"]
                self.assertIn(role, prompt)
                self.assertNotIn("{{", prompt)
                self.assertNotIn("finish`", prompt)
                native = resolved["options"]["native"]
                if name == "librarian":
                    self.assertIsNone(resolved["options"]["reasoning_effort"])
                    self.assertEqual(native, {"openai-responses.reasoning_enabled": False})
                    self.assertIn("read-only GitHub provider", prompt)
                    self.assertIn("host\npermits only GET requests", prompt)
                    self.assertNotIn("gh api", prompt)
                    self.assertNotIn("`shell`", prompt)
                    self.assertNotIn("`read_output`", prompt)
                else:
                    self.assertEqual(native, {})

    def test_every_prompt_names_exactly_the_tools_it_assembles(self) -> None:
        for name in NAMES:
            with self.subTest(name=name):
                _, resolved = P.show(name)
                prompt, held, face = resolved["system_prompt"], modules(resolved), faces(resolved)
                self.assertNotIn("{{", prompt)
                self.assertNotIn("finish", held)
                self.assertNotIn("# Finishing", prompt)
                ticks = backticked(prompt)
                for module in TOOL_MODULES:
                    if module in held:
                        self.assertIn(face[module], ticks, module)
                    else:
                        self.assertNotIn(module, ticks, module)
                if any(module in held for module in WORKER_MODULES):
                    self.assertIn("# Workers (optional)", prompt)
                else:
                    self.assertNotIn("# Workers", prompt)

    def test_every_template_names_its_tools_by_placeholder(self) -> None:
        # A literal tool name in a template is wrong whether or not the environment has the
        # tool: the placeholder renders the assembled name, and a section drops what is absent.
        for path in sorted(ENVIRONMENTS.glob("*/prompt.md")):
            name = path.parent.name
            with self.subTest(name=name):
                spec = tomllib.loads((path.parent / "environment.toml").read_text("utf-8"))
                listed = [tool["module"] for tool in spec.get("tools", [])]
                ticks = backticked(path.read_text("utf-8"))
                for module in TOOL_MODULES:
                    self.assertNotIn(module, ticks, f"{name} hard-codes `{module}`")
                    if module in listed:
                        self.assertIn("{{tool:" + module + "}}", ticks, module)


if __name__ == "__main__":
    unittest.main()
