#!/usr/bin/env python3
"""Fetches the pinned connector release into a directory — stdlib only.

    scripts/fetch-connectors.py --out <dir> [--lock connectors.lock] [--base-url <url>]
    scripts/fetch-connectors.py --pin <tag> [--lock connectors.lock] [--base-url <url>]

The subscription connectors live in their own repository (#893, ADR-0215). The distribution
composes one tagged release of it, pinned in connectors.lock by tag and by the SHA-256 of every
asset: a catalog archive (routes/ and profiles/, the catalog root the binary anchors) and one
package archive per connector module (`p1 modules install` reads them, ADR-0209). Fetching
downloads each pinned asset, refuses any whose digest differs and writes <dir>:

    <dir>/catalog/          the connectors' catalog root (routes/, profiles/)
    <dir>/packages/         the package archives (*.p1pkg), as released
    <dir>/modules/<name>/   each package unpacked into the layout scripts/build-modules.sh
                            publishes (<name>.wasm, <name>.manifest.json, <name>.sha256; <name>
                            is the package name with `/` as `-`), which the build outputs and
                            a share take
    <dir>/connectors.lock   the lock it was fetched by, written last

A <dir> whose connectors.lock equals the lock is up to date and is left as it is, without a
download; another earlier fetch is replaced whole. scripts/build-modules.sh --all fetches into
modules/target/connectors.

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
import json
import os
import shutil
import pathlib
import re
import sys
import tarfile
import tomllib
import urllib.request

CATALOG = "connectors-catalog.tar.gz"
# The catalog archive carries data only: the two directories a catalog root holds.
CATALOG_DIRS = ("routes/", "profiles/")
# A package archive (docs/design/modules/package.md, "Package archives"): exactly these members,
# its manifest the build manifest plus these two fields.
PACKAGE_FORMAT = "p1-module-package/1"
PACKAGE_MEMBERS = {"manifest.json", "component.wasm"}
ARCHIVE_ONLY = ("format", "version")
NAME = re.compile(r"[a-z0-9][a-z0-9-]*/[a-z0-9][a-z0-9-]*")
STAMP = "connectors.lock"
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


def unpack_package(asset: str, data: bytes, out: pathlib.Path) -> str:
    """Unpacks one package archive into <out>/<name>/ as the build publishes a package and
    returns <name>; an archive that is not exactly the frozen two members, whose manifest is
    not the package format's, or whose component differs from its manifest, is refused."""
    members = {}
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
            for member in archive.getmembers():
                if not member.isfile() or member.name not in PACKAGE_MEMBERS or member.name in members:
                    raise FetchError(f"{asset}: member {member.name!r} is not a package archive's")
                members[member.name] = archive.extractfile(member).read()
    except tarfile.TarError as error:
        raise FetchError(f"{asset}: {error}") from None
    if set(members) != PACKAGE_MEMBERS:
        raise FetchError(f"{asset}: a package archive holds exactly {sorted(PACKAGE_MEMBERS)}")
    try:
        manifest = json.loads(members["manifest.json"])
    except ValueError as error:
        raise FetchError(f"{asset}: manifest.json: {error}") from None
    if not isinstance(manifest, dict) or manifest.get("format") != PACKAGE_FORMAT:
        raise FetchError(f"{asset}: manifest.json is not {PACKAGE_FORMAT}")
    name = manifest.get("name")
    if not isinstance(name, str) or not NAME.fullmatch(name) or name.startswith("p1/"):
        raise FetchError(f"{asset}: package name {name!r} is not <namespace>/<name> outside p1/")
    component = members["component.wasm"]
    if manifest.get("digest") != "sha256:" + hashlib.sha256(component).hexdigest() \
            or manifest.get("size") != len(component):
        raise FetchError(f"{asset}: component.wasm differs from the digest and size its manifest pins")
    build = {key: value for key, value in manifest.items() if key not in ARCHIVE_ONLY}
    directory = name.replace("/", "-")
    target = out / directory
    if target.exists():
        raise FetchError(f"{asset}: package {name} is in the release twice")
    target.mkdir(parents=True)
    (target / f"{directory}.wasm").write_bytes(component)
    (target / f"{directory}.manifest.json").write_text(json.dumps(build, indent=2) + "\n")
    (target / f"{directory}.sha256").write_text(
        f"{hashlib.sha256(component).hexdigest()}  {directory}.wasm\n")
    return directory


def fetch(lock_path: pathlib.Path, out: pathlib.Path, override: str | None) -> str:
    lock = read_lock(lock_path)
    pinned = lock_path.read_bytes()
    if (out / STAMP).is_file() and (out / STAMP).read_bytes() == pinned:
        return f"{lock['repository']} {lock['tag']}: {out} is up to date"
    if out.exists() and any(out.iterdir()) and not (out / STAMP).is_file():
        raise FetchError(f"{out}: not empty and not an earlier fetch; fetch into a new directory")
    url = base_url(lock, override)
    fetched = {}
    for name, digest in sorted(lock["assets"].items()):
        data = download(f"{url}/{name}")
        actual = "sha256:" + hashlib.sha256(data).hexdigest()
        if actual != digest:
            raise FetchError(f"{name}: digest {actual} differs from the pinned {digest}")
        fetched[name] = data
    staging = out.with_name(f".{out.name}.{os.getpid()}.tmp")
    shutil.rmtree(staging, ignore_errors=True)
    try:
        files = unpack_catalog(fetched.pop(CATALOG), staging / "catalog")
        (staging / "packages").mkdir(parents=True)
        (staging / "modules").mkdir()
        for name, data in fetched.items():
            (staging / "packages" / name).write_bytes(data)
            unpack_package(name, data, staging / "modules")
        (staging / STAMP).write_bytes(pinned)
        shutil.rmtree(out, ignore_errors=True)
        os.replace(staging, out)
    finally:
        shutil.rmtree(staging, ignore_errors=True)
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
