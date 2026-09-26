#!/usr/bin/env python3
"""Writes modules/manifest.json into a staged p1 share archive — stdlib only.

    scripts/release-manifest.py --root <repository root> --commit <sha> [--tag <tag>]
                                --native <binary file> --modules-dir <staged modules dir>
                                [--build-modules-dir <built packages dir>]

The manifest binds the released binary, the source commit, the toolchain and runtime
pins, the WIT and schema digests and every staged package file, so an installer can
verify what it unpacks: S7's installer reads exactly this file. The staged `packages/`
tree holds the module packages; the freeze tag `wasm-boundary-v1` fixes the `components`
entry shape, so with `--build-modules-dir` the frozen fields come from the build outputs'
`<package>.manifest.json` and each entry is checked against the staged bytes.
`environment_locks` has no frozen entry shape, so it stays empty, and an absent pin is
null, never invented. The asset name is the native file's name, so the manifest cannot
claim a name the archive does not carry.

The pins file is data: it is parsed line by line and never sourced or executed. A
missing pins file is refused (scripts/module-toolchain.sh reads it the same way) while a
pin the file does not name is null; a staged packages tree is refused wholesale when it
carries a symlink, a special file or a `.cwasm` compiled-cache blob, because no
precompiled component is ever shipped.

Exit codes: 0 on success, 1 on a rejected input, 2 on a usage error.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import subprocess
import sys

FORMAT = "p1-release-manifest/1"

COMMIT_RE = re.compile(r"\A[0-9a-f]{40}\Z")

# The frozen `components` entry shape (crates/p1-module-runtime/src/manifest.rs refuses an
# unknown field): the package's four frozen manifest fields, its name and digest, plus the
# staged path this generator computes.
PACKAGE_FIELDS = ("name", "digest", "kind", "world", "protocol", "capabilities", "variant")
COMPONENT_FIELDS = ("name", "digest", "path", "kind", "world", "protocol", "capabilities",
                    "variant")
# The shape scripts/module-toolchain.sh accepts: a KEY=value line with one token of value.
PIN_LINE_RE = re.compile(r"\A([A-Z][A-Z0-9_]*)=(\S+)\Z")

# pin name -> (manifest section, field)
PIN_FIELDS = {
    "WASM_TARGET": ("toolchain", "wasm_target"),
    "WASM_TOOLS": ("toolchain", "wasm_tools"),
    "WIT_BINDGEN": ("toolchain", "wit_bindgen"),
    "WASMTIME": ("runtime", "wasmtime"),
    "WASMTIME_FEATURES": ("runtime", "wasmtime_features"),
}


class ManifestError(Exception):
    """A rejected input; the message names the file or flag and what was wrong."""


def sha256_file(path: str) -> str:
    """Hex SHA-256 of a file, read in chunks so a large payload stays off the heap."""
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def check_relpath(path: str, what: str) -> str:
    """Return a POSIX relative path, or refuse it: the manifest names paths, not places.

    A consumer resolves every entry below the archive root, so an absolute path, a
    backslash, an empty component or a `.`/`..` component would name something the
    archive does not hold.
    """
    if not path or path.startswith("/") or "\\" in path:
        raise ManifestError(f"{what}: {path!r} is not a POSIX relative path")
    if any(part in ("", ".", "..") for part in path.split("/")):
        raise ManifestError(f"{what}: {path!r} has an empty, '.' or '..' component")
    return path


def parse_pins(path: str) -> dict[str, str]:
    """Parse KEY=value lines into a mapping; the file is read, never sourced."""
    try:
        with open(path, encoding="utf-8") as handle:
            lines = list(handle)
    except OSError as exc:
        raise ManifestError(f"{path}: {exc.strerror}") from exc
    pins: dict[str, str] = {}
    for lineno, raw in enumerate(lines, 1):
        line = raw.rstrip("\r\n")
        if not line or line.startswith("#"):
            continue
        match = PIN_LINE_RE.match(line)
        if match is None:
            raise ManifestError(f"{path}:{lineno}: not a KEY=value line")
        pins[match.group(1)] = match.group(2)
    return pins


def tool_version(tool: str) -> str | None:
    """`<tool> -V` from PATH, or null when the tool is not there to answer."""
    try:
        proc = subprocess.run(
            [tool, "-V"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
            timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        raise ManifestError(f"{tool} -V exited {proc.returncode}")
    return proc.stdout.strip() or None


def walk_packages(packages_dir: str) -> list[tuple[str, str, int]]:
    """Return (path, absolute path, size) for every regular file below packages_dir.

    Symlinks, FIFOs, sockets and devices are refused rather than skipped: a package the
    manifest cannot hash must not reach the archive, and a symlink could name a file
    outside it. Directories are walked explicitly so a symlinked directory is refused
    instead of being silently ignored. A missing directory is the scaffold stage, where
    the package set is still empty.
    """
    if not os.path.isdir(packages_dir):
        return []

    found: list[tuple[str, str, int]] = []

    def walk(directory: str, prefix: str) -> None:
        for name in sorted(os.listdir(directory)):
            full = os.path.join(directory, name)
            rel = f"{prefix}/{name}"
            if name.endswith(".cwasm"):
                raise ManifestError(
                    f"{rel}: a compiled-cache blob is never shipped"
                )
            info = os.lstat(full)
            mode = info.st_mode
            if stat.S_ISLNK(mode):
                raise ManifestError(f"{rel}: symlink under packages/")
            if stat.S_ISDIR(mode):
                walk(full, rel)
            elif stat.S_ISREG(mode):
                found.append((rel, full, info.st_size))
            else:
                raise ManifestError(f"{rel}: special file under packages/")

    walk(packages_dir, "packages")
    return found


def digest_entries(
    root: str, directory: str, suffix: str, what: str, *, recursive: bool = True
) -> list[dict[str, str]]:
    """Digest every regular file ending in `suffix` below `directory`, sorted by path.

    `recursive` is False for the schema set, which the format fixes at one level
    (schema/*.json — only the WIT set is **/*.wit), so a future subdirectory under
    schema/ contributes nothing the installer would have to expect. A missing directory
    is the scaffold stage, where that set is still empty.
    """
    if not os.path.isdir(directory):
        return []

    found: list[tuple[str, str]] = []
    if recursive:
        for dirpath, dirnames, filenames in os.walk(directory):
            dirnames.sort()
            found.extend((dirpath, name) for name in sorted(filenames))
    else:
        found.extend((directory, name) for name in sorted(os.listdir(directory)))

    entries: list[dict[str, str]] = []
    for dirpath, name in found:
        if not name.endswith(suffix):
            continue
        full = os.path.join(dirpath, name)
        if os.path.islink(full) or not os.path.isfile(full):
            raise ManifestError(f"{full}: {what} is not a regular file")
        rel = check_relpath(os.path.relpath(full, root).replace(os.sep, "/"), what)
        entries.append({"path": rel, "sha256": sha256_file(full)})
    entries.sort(key=lambda entry: str(entry["path"]))
    return entries


def component_entries(build_dir: str, modules_dir: str) -> list[dict[str, object]]:
    """The `components` entries of the staged packages, or refuse an input.

    The staged `packages/` tree holds only what the frozen package format ships (today the
    `.wasm`), so the frozen fields come from the build outputs' `<package>.manifest.json`:
    each entry names the package, pins the staged file by the digest of its bytes and carries
    the fields the loader reads. A build output that names no staged file, or whose digest
    disagrees with the staged bytes, is refused rather than published.
    """
    if not os.path.isdir(build_dir):
        raise ManifestError(f"--build-modules-dir {build_dir}: not a directory")

    entries: list[dict[str, object]] = []
    seen: set[str] = set()
    for directory in sorted(os.listdir(build_dir)):
        package_dir = os.path.join(build_dir, directory)
        if os.path.islink(package_dir) or not os.path.isdir(package_dir):
            raise ManifestError(f"{directory}: a build output is not a directory")
        manifests = sorted(
            name for name in os.listdir(package_dir) if name.endswith(".manifest.json")
        )
        if len(manifests) != 1:
            raise ManifestError(
                f"{directory}: expected exactly one *.manifest.json, found {len(manifests)}"
            )
        manifest_path = os.path.join(package_dir, manifests[0])
        try:
            with open(manifest_path, encoding="utf-8") as handle:
                package = json.load(handle)
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ManifestError(f"{manifest_path}: {exc}") from exc
        if not isinstance(package, dict):
            raise ManifestError(f"{manifest_path}: not a JSON object")

        fields: dict[str, object] = {}
        for field in PACKAGE_FIELDS:
            if field not in package:
                raise ManifestError(f"{manifest_path}: missing {field}")
            fields[field] = package[field]
        for field in ("name", "kind", "world", "protocol", "variant"):
            if not isinstance(fields[field], str) or not fields[field]:
                raise ManifestError(f"{manifest_path}: {field} is not a name")
        capabilities = fields["capabilities"]
        if not isinstance(capabilities, list) or not all(
            isinstance(item, str) for item in capabilities
        ):
            raise ManifestError(f"{manifest_path}: capabilities is not a list of names")

        name = str(fields["name"])
        if "/" not in name:
            raise ManifestError(f"{manifest_path}: name {name!r} is not <namespace>/<name>")
        if name in seen:
            raise ManifestError(f"{name}: listed twice under the build outputs")
        seen.add(name)

        file = name.replace("/", "-")
        rel = check_relpath(f"packages/{file}/{file}.wasm", "component entry")
        staged = os.path.join(modules_dir, rel)
        if os.path.islink(staged) or not os.path.isfile(staged):
            raise ManifestError(f"{rel}: {manifest_path} names no staged regular file")
        digest = fields["digest"]
        actual = f"sha256:{sha256_file(staged)}"
        if not isinstance(digest, str) or digest != actual:
            raise ManifestError(
                f"{rel}: the staged bytes are {actual}, {manifest_path} says {digest!r}"
            )
        entries.append(
            {
                "name": name,
                "digest": actual,
                "path": rel,
                "kind": fields["kind"],
                "world": fields["world"],
                "protocol": fields["protocol"],
                "capabilities": list(capabilities),
                "variant": fields["variant"],
            }
        )

    entries.sort(key=lambda entry: str(entry["name"]))
    return entries


def build_manifest(args: argparse.Namespace) -> dict[str, object]:
    """Collect the manifest the staged archive describes, or refuse an input."""
    root = os.path.abspath(args.root)
    if not os.path.isdir(root):
        raise ManifestError(f"--root {args.root}: not a directory")

    if not COMMIT_RE.match(args.commit or ""):
        raise ManifestError(
            f"--commit {args.commit!r}: expected 40 lowercase hex characters"
        )

    if not os.path.isfile(args.native):
        raise ManifestError(f"--native {args.native}: missing")
    asset = check_relpath(os.path.basename(os.path.abspath(args.native)), "--native")

    modules_dir = os.path.abspath(args.modules_dir)
    if not os.path.isdir(modules_dir):
        raise ManifestError(f"--modules-dir {args.modules_dir}: not a directory")

    pins = parse_pins(os.path.join(root, "modules", "toolchain.pins"))

    toolchain: dict[str, str | None] = {
        "rustc": tool_version("rustc"),
        "cargo": tool_version("cargo"),
        "wasm_target": None,
        "wasm_tools": None,
        "wit_bindgen": None,
    }
    runtime: dict[str, str | None] = {"wasmtime": None, "wasmtime_features": None}
    for pin, (section, field) in PIN_FIELDS.items():
        (toolchain if section == "toolchain" else runtime)[field] = pins.get(pin)

    packages: list[dict[str, object]] = []
    seen: set[str] = set()
    for rel, full, size in walk_packages(os.path.join(modules_dir, "packages")):
        path = check_relpath(rel, "packages entry")
        if path in seen:
            raise ManifestError(f"{path}: duplicate path under packages/")
        seen.add(path)
        packages.append({"path": path, "sha256": sha256_file(full), "size": size})
    packages.sort(key=lambda entry: str(entry["path"]))

    components: list[dict[str, object]] = []
    if args.build_modules_dir:
        components = component_entries(
            os.path.abspath(args.build_modules_dir), modules_dir
        )
        # Every staged package file must be a component the runtime can load by name, and
        # every component must name a file that is there: publishing one without the other
        # would ship bytes no manifest binds, or an entry no archive carries.
        staged = {str(entry["path"]) for entry in packages}
        named = {str(entry["path"]) for entry in components}
        if staged != named:
            detail = []
            if staged - named:
                detail.append(
                    "without a component entry: " + ", ".join(sorted(staged - named))
                )
            if named - staged:
                detail.append(
                    "without a staged file: " + ", ".join(sorted(named - staged))
                )
            raise ManifestError(
                "the staged packages and the built components disagree: "
                + "; ".join(detail)
            )

    return {
        "format": FORMAT,
        "commit": args.commit,
        "tag": args.tag or None,
        "native": {"asset": asset, "sha256": sha256_file(args.native)},
        "toolchain": toolchain,
        "runtime": runtime,
        "wit": digest_entries(
            root, os.path.join(root, "modules", "wit"), ".wit", "wit entry"
        ),
        "schemas": digest_entries(
            root,
            os.path.join(root, "crates", "p1-module-protocol", "schema"),
            ".json",
            "schema entry",
            recursive=False,
        ),
        "packages": packages,
        # The freeze tag wasm-boundary-v1 fixes the component entry shape; without a build
        # output directory there is nothing to fill it from, so it stays empty.
        "components": components,
        # No entry shape is frozen for the environment locks; leave it empty.
        "environment_locks": [],
    }


def write_manifest(modules_dir: str, manifest: dict[str, object]) -> str:
    """Write the manifest with sorted keys, two-space indent and a trailing newline.

    Equal inputs then give byte-identical output, so two builds of one commit publish
    the same manifest bytes.
    """
    path = os.path.join(os.path.abspath(modules_dir), "manifest.json")
    text = json.dumps(manifest, sort_keys=True, indent=2) + "\n"
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
    return path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="release-manifest.py",
        description="Write modules/manifest.json into a staged p1 share archive.",
    )
    parser.add_argument("--root", required=True, help="repository root")
    parser.add_argument(
        "--commit", required=True, help="40-character lowercase hex source commit"
    )
    parser.add_argument(
        "--tag",
        default=None,
        help="release tag, e.g. main-<12 hex>; omit it for a candidate build",
    )
    parser.add_argument("--native", required=True, help="the released binary file")
    parser.add_argument(
        "--modules-dir", required=True, help="staged archive modules directory"
    )
    parser.add_argument(
        "--build-modules-dir",
        default=None,
        help="built package outputs (scripts/build-modules.sh), for the components entries",
    )
    args = parser.parse_args(argv)

    try:
        manifest = build_manifest(args)
        path = write_manifest(args.modules_dir, manifest)
    except ManifestError as exc:
        print(f"release-manifest: {exc}", file=sys.stderr)
        return 1
    print(f"release-manifest: wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
