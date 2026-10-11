#!/usr/bin/env python3
"""Merge release catalog roots without overwriting a relative file path (ADR-0196)."""
from __future__ import annotations

import argparse
import os
import shutil
import sys


DIRECTORIES = ("environments", "routes", "profiles", "accounts")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--catalog-root", action="append")
    args = parser.parse_args()
    seen: dict[str, str] = {}
    try:
        for directory in DIRECTORIES:
            os.makedirs(os.path.join(args.out, directory), exist_ok=True)
        for root in args.catalog_root or ["."]:
            if not root or ":" in root:
                raise ValueError("catalog root cannot be empty or contain ':'")
            root = os.path.join(args.repo_root, root)
            if not os.path.isdir(root):
                raise ValueError(f"catalog root {root}: not a directory")
            for directory in DIRECTORIES:
                source = os.path.join(root, directory)
                if not os.path.isdir(source):
                    continue
                for path, directories, files in os.walk(source, followlinks=False):
                    # Preserve links for the installer's existing archive checks, but do
                    # not follow a linked directory while merging roots.
                    links = [name for name in directories
                             if os.path.islink(os.path.join(path, name))]
                    directories[:] = [name for name in directories if name not in links]
                    relative_dir = os.path.relpath(path, root)
                    if relative_dir in seen:
                        raise ValueError(f"catalog path conflict {relative_dir}: "
                                         f"{seen[relative_dir]} and {path}")
                    os.makedirs(os.path.join(args.out, relative_dir), exist_ok=True)
                    for name in files + links:
                        full = os.path.join(path, name)
                        relative = os.path.join(relative_dir, name)
                        if relative in seen:
                            raise ValueError(f"duplicate catalog path {relative}: "
                                             f"{seen[relative]} and {full}")
                        seen[relative] = full
                        destination = os.path.join(args.out, relative)
                        if os.path.lexists(destination):
                            raise ValueError(f"catalog path conflict {relative}: "
                                             f"{destination} and {full}")
                        shutil.copy2(full, destination, follow_symlinks=False)
        return 0
    except (OSError, ValueError) as error:
        print(f"catalog-roots: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
