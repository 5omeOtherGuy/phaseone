#!/usr/bin/env python3
"""The bare product's share and ACP smoke (ADR-0200, ADR-0214, ADR-0215) — stdlib only.

    scripts/bare-smoke.py --p1 <p1 binary> --share <the bare share archive>

Unpacks the share `scripts/stage-release.sh --parts ""` staged into a scratch directory and
checks it holds the core alone: no connectors package in its module set and no route of a part.
Then runs the binary against that catalog alone with a throwaway home and configuration
directory, so no user file, credential or pack is read: `p1 env check` must pass every
environment, `p1 env show default` must resolve the `default` environment, and `p1 acp` must
answer `initialize` and `session/new` over stdio. No provider is reached: nothing here sends a
prompt.
"""
from __future__ import annotations

import argparse
import json
import pathlib
import select
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent

ANSWER_SECONDS = 60


def fail(message: str) -> None:
    print(f"bare smoke: {message}", file=sys.stderr)
    sys.exit(1)


def stage(share: pathlib.Path, catalog: pathlib.Path) -> None:
    """Unpack the share into `catalog` and refuse a part's file in it."""
    with tarfile.open(share, "r:gz") as archive:
        archive.extractall(catalog, filter="data")
    connector = sorted(path.name for path in (catalog / "modules/packages").glob("connectors-*"))
    if connector:
        fail(f"the bare share ships connector packages: {', '.join(connector)}")
    # The connectors' routes, from the pinned release scripts/build-modules.sh --all fetched.
    connector_routes = ROOT / "modules/target/connectors/catalog/routes"
    if not connector_routes.is_dir():
        fail(f"{connector_routes} is missing: run scripts/build-modules.sh --all")
    part_routes = {path.name for path in connector_routes.glob("*.toml")}
    shipped = sorted(path.name for path in (catalog / "routes").glob("*.toml")
                     if path.name in part_routes)
    if shipped:
        fail(f"the bare share ships a part's routes: {', '.join(shipped)}")


def run(p1: str, env: dict[str, str], cwd: pathlib.Path, *args: str) -> str:
    result = subprocess.run([p1, *args], env=env, cwd=cwd, capture_output=True, text=True,
                            timeout=300, check=False)
    if result.returncode != 0:
        fail(f"`p1 {' '.join(args)}` exited {result.returncode}:\n{result.stdout}{result.stderr}")
    return result.stdout


def acp_session(p1: str, env: dict[str, str], cwd: pathlib.Path) -> str:
    child = subprocess.Popen([p1, "acp"], env=env, cwd=cwd, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    assert child.stdin is not None and child.stdout is not None

    def call(number: int, method: str, params: dict) -> dict:
        request = {"jsonrpc": "2.0", "id": number, "method": method, "params": params}
        child.stdin.write(json.dumps(request) + "\n")
        child.stdin.flush()
        deadline = time.monotonic() + ANSWER_SECONDS
        while time.monotonic() < deadline:
            ready, _, _ = select.select([child.stdout], [], [], 1)
            if not ready:
                continue
            line = child.stdout.readline()
            if not line:
                break
            message = json.loads(line)
            if message.get("id") == number and "method" not in message:
                if "error" in message:
                    fail(f"`{method}` answered an error: {message['error']}")
                return message["result"]
        fail(f"no answer to `{method}` within {ANSWER_SECONDS}s")

    try:
        call(0, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        session = call(1, "session/new", {"cwd": str(cwd), "mcpServers": []})
    finally:
        child.stdin.close()
        try:
            child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    model = next((option.get("currentValue") for option in session.get("configOptions", [])
                  if option.get("id") == "model"), None)
    if not session.get("sessionId") or model is None or not model.startswith("default/"):
        fail(f"`session/new` opened no session on the default environment: {session}")
    return model


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--p1", required=True, help="the p1 binary to run")
    parser.add_argument("--share", required=True, help="the bare share archive")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="p1-bare-smoke-") as temp:
        scratch = pathlib.Path(temp)
        catalog, home, config, work = (scratch / name for name in
                                       ("catalog", "home", "config", "work"))
        for directory in (home, config, work):
            directory.mkdir()
        stage(pathlib.Path(args.share), catalog)
        env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "P1_CONFIG_DIR": str(config),
               "XDG_CONFIG_HOME": str(config), "TMPDIR": temp,
               "P1_ENVIRONMENTS_DIR": str(catalog / "environments")}
        checked = run(args.p1, env, work, "env", "check")
        run(args.p1, env, work, "env", "show", "default")
        model = acp_session(args.p1, env, work)
    print(f"bare smoke: {checked.strip().splitlines()[-1]}; "
          f"`env show default` resolved; ACP session opened on {model}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
