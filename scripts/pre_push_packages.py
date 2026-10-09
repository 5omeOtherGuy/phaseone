#!/usr/bin/env python3
"""Select pre-push test packages from Cargo metadata on stdin and changed paths.

Shipped-data readers are derived from Rust source/test/build-script path literals,
not a package inventory. This is deliberately conservative, not a Rust parser:
path references in inactive code also select their package.
"""
import json
from pathlib import Path
import re
import sys


def main() -> None:
    metadata = json.load(sys.stdin)
    root = Path(metadata["workspace_root"])
    changed = [Path(name) for name in sys.argv[1:]]
    packages = [
        package for package in metadata["packages"]
        if package["id"] in metadata["workspace_members"]
    ]
    directories = {
        path.parts[0] for path in changed if len(path.parts) > 1
        # Code, build infrastructure and design docs are not shipped data dirs.
        if path.parts[0] not in {"crates", "modules", "scripts", "docs"}
        and not path.parts[0].startswith(".")
    }
    selected = set()
    for path in changed:
        owners = [
            package for package in packages
            if (root / path).is_relative_to(Path(package["manifest_path"]).parent)
        ]
        if owners:
            selected.add(max(owners, key=lambda p: len(p["manifest_path"]))["name"])
        if path.parts and path.parts[0] == "modules":
            selected.add("p1-module-tests")

    for package in packages:
        if not directories:
            break
        directory = Path(package["manifest_path"]).parent
        sources = [directory / "build.rs"]
        for subtree in ("src", "tests"):
            sources.extend((directory / subtree).rglob("*.rs"))
        for source in sources:
            if not source.is_file():
                continue
            text = source.read_text(encoding="utf-8")
            for data in directories:
                name = re.escape(data)
                # Relative include paths, concat!/format! paths, and repo-root joins.
                path_literal = rf'"(?:/?(?:\.\./)+{name}(?:/[^"\n]*)?|{name}/[^"\n]*)"'
                root_join = rf'(?:join|repo|shipped|new|read_dir)\s*\(\s*"{name}(?:/[^"\n]*)?"'
                if re.search(path_literal, text) or re.search(root_join, text):
                    selected.add(package["name"])
                    break
            if package["name"] in selected:
                break
    for name in sorted(selected):
        print(name)


if __name__ == "__main__":
    main()
