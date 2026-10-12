#!/usr/bin/env python3
"""Packs one built module package into a package archive — stdlib only.

    scripts/pack-module.py <package build dir> --version <version> --output <file.p1pkg>

The build dir is what scripts/build-modules.sh publishes for one package
(modules/target/p1-modules/<package>/): <package>.wasm and <package>.manifest.json. The
archive (docs/design/modules/package.md, "Package archives", ADR-0209) is an uncompressed
ustar file of exactly two members: manifest.json, the build manifest plus `format`
(p1-module-package/1) and `version`, and component.wasm, the component. `p1 modules install`
reads it. The component's SHA-256 and size are checked against the build manifest first, so
an archive never pins bytes it does not carry. Members carry no owner, no mode beyond 0644
and mtime 0, so one build packs to the same bytes everywhere.

Exit codes: 0 on success, 1 on a rejected input, 2 on a usage error.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import sys
import tarfile

FORMAT = "p1-module-package/1"
# The build manifest's fields, in the order the archive's manifest lists them.
BUILD_FIELDS = ("name", "kind", "world", "protocol", "capabilities", "variant", "digest", "size")
# A credential method's or a usage probe's build manifest also names its token origins
# (ADR-0212, ADR-0214).
CREDENTIAL_FIELDS = ("token_origins",)
SENDS_CREDENTIAL = ("credential-method", "usage-probe")


class PackError(Exception):
    """A rejected input; the message names the file and what was wrong."""


def read_build(directory: str) -> tuple[dict, bytes]:
    """The build manifest and component bytes of the package built into `directory`."""
    package = os.path.basename(os.path.normpath(directory))
    manifest_path = os.path.join(directory, f"{package}.manifest.json")
    wasm_path = os.path.join(directory, f"{package}.wasm")
    try:
        with open(manifest_path, encoding="utf-8") as handle:
            manifest = json.load(handle)
        with open(wasm_path, "rb") as handle:
            component = handle.read()
    except (OSError, ValueError) as error:
        raise PackError(f"{directory}: not a built package: {error}") from error
    fields = BUILD_FIELDS
    if isinstance(manifest, dict) and manifest.get("kind") in SENDS_CREDENTIAL:
        fields = BUILD_FIELDS + CREDENTIAL_FIELDS
    if not isinstance(manifest, dict) or set(manifest) != set(fields):
        raise PackError(f"{manifest_path}: expected exactly the fields {', '.join(fields)}")
    digest = "sha256:" + hashlib.sha256(component).hexdigest()
    if manifest["digest"] != digest:
        raise PackError(f"{wasm_path}: the manifest pins {manifest['digest']}, the bytes are {digest}")
    if manifest["size"] != len(component):
        raise PackError(f"{wasm_path}: the manifest says {manifest['size']} bytes, the file has {len(component)}")
    return manifest, component


def archive(manifest: dict, version: str, component: bytes) -> bytes:
    """The package archive's bytes."""
    package_manifest = {"format": FORMAT, "name": manifest["name"], "version": version}
    package_manifest.update((field, manifest[field]) for field in BUILD_FIELDS if field != "name")
    package_manifest.update((field, manifest[field]) for field in CREDENTIAL_FIELDS if field in manifest)
    members = [
        ("manifest.json", (json.dumps(package_manifest, indent=2) + "\n").encode()),
        ("component.wasm", component),
    ]
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o644
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            tar.addfile(info, io.BytesIO(data))
    return out.getvalue()


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Pack one built module package into a .p1pkg archive.")
    parser.add_argument("directory", help="the package's build output directory")
    parser.add_argument("--version", required=True, help="the version the package is published at")
    parser.add_argument("--output", required=True, help="the archive to write")
    args = parser.parse_args(argv)
    if not args.version.strip():
        print("pack-module: --version is empty", file=sys.stderr)
        return 2
    try:
        manifest, component = read_build(args.directory)
    except PackError as error:
        print(f"pack-module: {error}", file=sys.stderr)
        return 1
    data = archive(manifest, args.version, component)
    staging = f"{args.output}.{os.getpid()}.tmp"
    with open(staging, "wb") as handle:
        handle.write(data)
    os.replace(staging, args.output)
    print(f"pack-module: {manifest['name']} {manifest['digest']} -> {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
