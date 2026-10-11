#!/usr/bin/env python3
"""Select pre-push test packages from Cargo metadata on stdin and changed paths.

Shipped-data readers are derived from Rust source/test/build-script path literals,
not a package inventory. This is deliberately conservative, not a Rust parser:
path references in inactive code also select their package. Only reads of the checkout
count: a literal anchored at the checkout (a `../` path, a `CARGO_MANIFEST_DIR`, `repo`,
`checkout` or `shipped` receiver, or the `repo`/`shipped`/`read_dir`/`Path::new` helpers).
A path a test builds under a temporary catalog, such as `scratch.join("routes")` or
`format!("routes/{id}.toml")`, selects nothing (#843).
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
                literal = rf'(?:&?format!\s*\(\s*)?"{name}(?:/[^"\n]*)?"'
                # Relative include and concat!/format! paths.
                relative = rf'"/?(?:\.\./)+{name}(?:/[^"\n]*)?"'
                # The checkout helpers a test names.
                helper = rf'(?:repo|shipped|read_dir|new)\s*\(\s*{literal}'
                # A join chained from a checkout root within one expression.
                rooted = rf'(?:\.\./|CARGO_MANIFEST_DIR|repo|checkout|shipped)[^;{{}}]*?\.\s*join\s*\(\s*{literal}'
                if any(re.search(pattern, text) for pattern in (relative, helper, rooted)):
                    selected.add(package["name"])
                    break
            if package["name"] in selected:
                break
    for name in sorted(selected):
        print(name)


if __name__ == "__main__":
    main()
