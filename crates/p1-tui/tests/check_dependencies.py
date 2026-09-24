"""Resolved-graph regression check for the Iris presentation-only migration.

Run with: python3 crates/p1-tui/tests/check_dependencies.py
Uses locked/offline Cargo metadata, not compilation or live network.
"""

import json
from pathlib import Path
import subprocess
import unittest


class PresentationDependencies(unittest.TestCase):
    def test_resolved_normal_dependencies_stay_presentation_only(self):
        root = Path(__file__).resolve().parents[3]
        metadata = json.loads(
            subprocess.check_output(
                ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
                cwd=root,
                text=True,
            )
        )
        packages = {package["id"]: package for package in metadata["packages"]}
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        tui = next(key for key, value in packages.items() if value["name"] == "p1-tui")

        def dependencies(key):
            return [
                dep["pkg"]
                for dep in nodes[key]["deps"]
                if any(kind["kind"] != "dev" for kind in dep["dep_kinds"])
            ]

        self.assertEqual(
            {packages[key]["name"] for key in dependencies(tui)},
            {"p1-contracts", "crossterm", "ratatui", "tokio", "unicode-width"},
        )
        pending = [tui]
        visited = set()
        while pending:
            key = pending.pop()
            if key not in visited:
                visited.add(key)
                pending.extend(dependencies(key))
        self.assertEqual(
            {
                packages[key]["name"]
                for key in visited
                if packages[key]["name"].startswith("p1-")
            },
            {"p1-tui", "p1-contracts"},
        )


if __name__ == "__main__":
    unittest.main()
