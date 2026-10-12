#!/usr/bin/env python3
"""Whether a pull request head may reuse an earlier green gate instead of a full run (#829).

    scripts/gate_reuse.py --head SHA --base-ref origin/main --repo OWNER/NAME

Prints `reuse=true` or `reuse=false` and the reason. A head reuses the green of an earlier
commit C of the same pull request when its code is exactly the clean three-way merge of C with
the main commit the head is now based on: merging main to resolve a Markdown conflict, or just
to catch up. That is the combination auto-merge already lands untested, since main does not
require an up-to-date branch. Markdown under an `environments/` directory, the root's or a
pack's (#853), is runtime input (prompts), so it counts as code. Anything unreadable answers false, so the full gate runs.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from typing import Callable

# The earlier commits of a pull request that are asked for a green gate, newest first.
MAX_CANDIDATES = 30


def git(repo: str, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=check)


def is_code(path: str) -> bool:
    return not path.endswith(".md") or path.startswith("environments/") or "/environments/" in path


def decide(repo: str, head: str, base_ref: str, is_green: Callable[[str], bool]) -> tuple[bool, str]:
    head = git(repo, "rev-parse", head).stdout.strip()
    base = git(repo, "merge-base", head, base_ref).stdout.strip()
    own = git(repo, "rev-list", f"--max-count={MAX_CANDIDATES + 1}", head, f"^{base_ref}").stdout.split()
    green = next((c for c in own if c != head and is_green(c)), None)
    if green is None:
        return False, "no earlier commit of this pull request has a green gate"
    merged = git(repo, "merge-tree", "--write-tree", "--name-only", "--no-messages", green, base,
                 check=False)
    if merged.returncode not in (0, 1):
        return False, f"git merge-tree failed: {merged.stderr.strip()[:200]}"
    tree, *conflicted = merged.stdout.split("\n")
    conflicted_code = [p for p in conflicted if p and is_code(p)]
    if conflicted_code:
        return False, f"{green[:12]} and main conflict in code: {', '.join(conflicted_code[:10])}"
    changed = git(repo, "diff", "--name-only", tree, head).stdout.split()
    changed_code = [p for p in changed if is_code(p)]
    if changed_code:
        return False, (f"code differs from the clean merge of green {green[:12]} with main "
                       f"{base[:12]}: {', '.join(changed_code[:10])}")
    return True, (f"code equals the clean merge of green {green[:12]} with main {base[:12]}; "
                  f"only Markdown differs ({len(changed)} file(s))")


def github_green(repo_slug: str) -> Callable[[str], bool]:
    def is_green(sha: str) -> bool:
        done = subprocess.run(
            ["gh", "api", f"repos/{repo_slug}/commits/{sha}/check-runs?check_name=gate&per_page=100"],
            capture_output=True, text=True)
        if done.returncode != 0:
            return False
        runs = json.loads(done.stdout).get("check_runs", [])
        return any(run.get("conclusion") == "success" for run in runs)
    return is_green


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--head", required=True)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--repo", required=True, help="OWNER/NAME on GitHub")
    parser.add_argument("--dir", default=".", help="the git checkout (default: .)")
    args = parser.parse_args(argv)
    try:
        reuse, reason = decide(args.dir, args.head, args.base_ref, github_green(args.repo))
    except (subprocess.CalledProcessError, ValueError) as error:
        reuse, reason = False, f"cannot decide: {error}"
    print(f"reuse={'true' if reuse else 'false'}")
    print(f"reason: {reason}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
