#!/usr/bin/env python3
"""Fetches the pinned connector release into a directory — stdlib only.

    scripts/fetch-connectors.py --out <dir> [--lock connectors.lock] [--base-url <url>]
    scripts/fetch-connectors.py --pin <tag> [--lock connectors.lock] [--base-url <url>]

The subscription connectors live in their own repository (#893). The distribution build composes
one tagged release of it, pinned in connectors.lock by tag and by the SHA-256 of every asset:
a catalog archive (routes/ and profiles/, the catalog root the binary anchors) and one package
archive per connector module (`p1 modules install` reads them, ADR-0209). Fetching downloads
each pinned asset, refuses any whose digest differs and unpacks into <dir>:

    <dir>/catalog/    the connectors' catalog root (routes/, profiles/)
    <dir>/packages/   the package archives (*.p1pkg)

--pin <tag> rewrites the lock to that release: it reads the release's SHA256SUMS and pins
every asset it lists. Pinning trusts that release's own sums once; every later fetch is
checked against the lock. --base-url replaces https://github.com/<repository>/releases/
download/<tag> (a file:// URL serves a local release, as the tests do).

Exit codes: 0 on success, 1 on a rejected or failed fetch, 2 on a usage error.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import os
import pathlib
import re
import sys
import tarfile
import tomllib
import urllib.request

CATALOG = "connectors-catalog.tar.gz"
# The catalog archive carries data only: the two directories a catalog root holds.
CATALOG_DIRS = ("routes/", "profiles/")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
ASSET = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
# A release asset is small; a larger download is refused rather than held in memory.
MAX_ASSET = 64 << 20


class FetchError(Exception):
    """A rejected lock, download or archive; the message names it and what was wrong."""


def read_lock(path: pathlib.Path) -> dict:
    try:
        lock = tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise FetchError(f"{path}: {error}") from None
    for key in ("repository", "tag"):
        if not isinstance(lock.get(key), str) or not lock[key]:
            raise FetchError(f"{path}: `{key}` must be a non-empty string")
    assets = lock.get("assets")
    if not isinstance(assets, dict) or CATALOG not in assets:
        raise FetchError(f"{path}: [assets] must pin {CATALOG} and the packages")
    for name, digest in assets.items():
        if not ASSET.fullmatch(name):
            raise FetchError(f"{path}: asset name {name!r} is not a plain file name")
        if name != CATALOG and not name.endswith(".p1pkg"):
            raise FetchError(f"{path}: asset {name} is neither {CATALOG} nor a .p1pkg")
        if not isinstance(digest, str) or not DIGEST.fullmatch(digest):
            raise FetchError(f"{path}: asset {name} must pin `sha256:<64 hex>`")
    return lock


def base_url(lock: dict, override: str | None) -> str:
    return (override or f"https://github.com/{lock['repository']}/releases/download/{lock['tag']}").rstrip("/")


def download(url: str) -> bytes:
    try:
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read(MAX_ASSET + 1)
    except OSError as error:
        raise FetchError(f"{url}: {error}") from None
    if len(data) > MAX_ASSET:
        raise FetchError(f"{url}: larger than {MAX_ASSET} bytes")
    return data


def unpack_catalog(data: bytes, out: pathlib.Path) -> int:
    """Unpacks the catalog archive's regular files under routes/ and profiles/; anything else
    in it (a link, a device, an absolute or `..` path, another directory) refuses the whole."""
    members = []
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive.getmembers():
            name = member.name.removeprefix("./")
            if member.isdir() and name.rstrip("/") + "/" in CATALOG_DIRS:
                continue
            parts = pathlib.PurePosixPath(name).parts
            if (not member.isfile() or name.startswith("/") or ".." in parts
                    or not name.startswith(CATALOG_DIRS) or len(parts) != 2):
                raise FetchError(f"{CATALOG}: member {member.name!r} is not a routes/ or profiles/ file")
            members.append((name, archive.extractfile(member).read()))
    for name, content in members:
        target = out / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)
    return len(members)


def fetch(lock_path: pathlib.Path, out: pathlib.Path, override: str | None) -> str:
    lock = read_lock(lock_path)
    url = base_url(lock, override)
    if out.exists() and any(out.iterdir()):
        raise FetchError(f"{out}: not empty; fetch into a new directory")
    fetched = {}
    for name, digest in sorted(lock["assets"].items()):
        data = download(f"{url}/{name}")
        actual = "sha256:" + hashlib.sha256(data).hexdigest()
        if actual != digest:
            raise FetchError(f"{name}: digest {actual} differs from the pinned {digest}")
        fetched[name] = data
    files = unpack_catalog(fetched.pop(CATALOG), out / "catalog")
    packages = out / "packages"
    packages.mkdir(parents=True)
    for name, data in fetched.items():
        (packages / name).write_bytes(data)
    return f"{lock['repository']} {lock['tag']}: {files} catalog files, {len(fetched)} packages"


def pin(lock_path: pathlib.Path, tag: str, override: str | None) -> str:
    repository = read_lock(lock_path)["repository"] if lock_path.exists() else None
    if not repository:
        raise FetchError(f"{lock_path}: missing; pinning needs its `repository`")
    url = base_url({"repository": repository, "tag": tag}, override)
    sums = {}
    for line in download(f"{url}/SHA256SUMS").decode().splitlines():
        digest, _, name = line.partition("  ")
        name = name.removeprefix("./")
        if not re.fullmatch(r"[0-9a-f]{64}", digest) or not ASSET.fullmatch(name):
            raise FetchError(f"SHA256SUMS of {tag}: unreadable line {line!r}")
        sums[name] = "sha256:" + digest
    if CATALOG not in sums:
        raise FetchError(f"SHA256SUMS of {tag}: no {CATALOG}")
    assets = [f'"{name}" = "{digest}"' for name, digest in sorted(sums.items())
              if name == CATALOG or name.endswith(".p1pkg")]
    lines = [line for line in lock_path.read_text().splitlines() if line.startswith("#")]
    lines += ["", f'repository = "{repository}"', f'tag = "{tag}"', "", "[assets]", *assets]
    lock_path.write_text("\n".join(lines).lstrip("\n") + "\n")
    read_lock(lock_path)
    return f"pinned {repository} {tag}: {len(assets)} assets"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--lock", type=pathlib.Path,
                        default=pathlib.Path(__file__).resolve().parent.parent / "connectors.lock")
    parser.add_argument("--base-url")
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--out", type=pathlib.Path)
    action.add_argument("--pin", metavar="TAG")
    args = parser.parse_args()
    try:
        if args.out:
            print("fetch-connectors: " + fetch(args.lock, args.out, args.base_url))
        else:
            print("fetch-connectors: " + pin(args.lock, args.pin, args.base_url))
    except FetchError as error:
        print(f"fetch-connectors: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
