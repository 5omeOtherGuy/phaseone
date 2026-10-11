#!/usr/bin/env python3
"""Run Cargo's complete default test set, scheduling independent binaries concurrently.

Cargo owns compilation, features, target discovery and doctests. No individual test is
filtered: each fresh executable runs its normal libtest harness, including its normal
ignored-test policy. The two partitions cover every workspace package; a new package
belongs to workspace automatically. Stdlib only; usable with the local Cargo wrapper.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
SKIPS = ("SKIP: bwrap unusable here", "SKIP: no usable XDG_RUNTIME_DIR here")
# First-run ordering from the audited gate. Later main caches carry observed durations.
# This affects only scheduling: every target in the Cargo inventory still runs.
BASELINE_SECONDS = {"lead_prompt_coherence": 365, "route_files": 315,
                    "anthropic_route": 126, "openai_route": 80, "cli": 41}


def label_of(target: tuple) -> str:
    key, _, cwd, _ = target
    return f"{cwd.name}/{key[1]} ({','.join(key[2])})"


def selection(partition: str) -> list[str]:
    return (["-p", "p1-module-tests"] if partition == "modules" else
            ["--workspace", "--exclude", "p1-module-tests"])


def test_targets(metadata: dict, partition: str) -> dict[tuple, tuple]:
    members = set(metadata["workspace_members"])
    targets = {}
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        if (package["name"] == "p1-module-tests") != (partition == "modules"):
            continue
        cwd = Path(package["manifest_path"]).parent
        # Cargo exports package metadata at execution too; the MCP child-environment
        # acceptance deliberately verifies that these variables are scrubbed.
        env = {"CARGO_MANIFEST_DIR": str(cwd), "CARGO_MANIFEST_PATH": package["manifest_path"],
               "CARGO_PKG_NAME": package["name"], "CARGO_PKG_VERSION": package["version"],
               "CARGO_TARGET_TMPDIR": str(Path(metadata["target_directory"]) / "tmp")}
        for field in ("description", "homepage", "repository", "license", "license_file", "rust_version"):
            env["CARGO_PKG_" + field.upper()] = package.get(field) or ""
        env["CARGO_PKG_AUTHORS"] = ":".join(package.get("authors", []))
        version, _, pre = package["version"].partition("-")
        env["CARGO_PKG_VERSION_PRE"] = pre
        for field, value in zip(("MAJOR", "MINOR", "PATCH"), version.split(".")):
            env["CARGO_PKG_VERSION_" + field] = value
        for target in package["targets"]:
            if target["test"] and "custom-build" not in target["kind"]:
                key = (package["id"], target["name"], tuple(target["kind"]))
                targets[key] = (cwd, env)
    return targets


def executables(messages: list[dict], expected: dict[tuple, tuple]) -> list[tuple]:
    found = {}
    for message in messages:
        if message.get("reason") != "compiler-artifact" or not message["profile"]["test"]:
            continue
        target = message["target"]
        key = (message["package_id"], target["name"], tuple(target["kind"]))
        if key in expected and message.get("executable"):
            cwd, env = expected[key]
            found[key] = (key, Path(message["executable"]), cwd, env)
    if set(found) != set(expected):
        raise ValueError(f"Cargo test inventory mismatch; missing: {sorted(set(expected) - set(found))}")
    if not found:
        raise ValueError("Cargo reported no test executables")
    return list(found.values())


def shard_targets(targets: list[tuple], shard: int, shards: int, costs: dict) -> list[tuple]:
    """Balance every target once using committed costs, identical on every runner.

    Cached timings may change while jobs start. They can order a runner's work, but
    cannot decide membership: differing restores would lose or duplicate coverage.
    """
    buckets = [[] for _ in range(shards)]
    loads = [0.0] * shards
    for target in sorted(targets, key=lambda t: (-costs.get(label_of(t), 1), label_of(t))):
        slot = min(range(shards), key=lambda i: (loads[i], len(buckets[i]), i))
        buckets[slot].append(target)
        loads[slot] += max(costs.get(label_of(target), 1), 0.01)
    return buckets[shard - 1]


def run_binary(target: tuple, log_dir: Path, timeout: int) -> tuple[str, int, float]:
    key, executable, cwd, env = target
    label = label_of(target)
    started = time.monotonic()
    with tempfile.TemporaryFile(mode="w+") as log:
        child = subprocess.Popen([str(executable), "--nocapture"], cwd=cwd,
                                 env={**os.environ, **env},
                                 stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        try:
            status = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
            status = 124
        log.seek(0)
        output = log.read()
    if any(marker in output for marker in SKIPS):
        status = status or 1
    # Keep one log per executable; passing output is not mixed across concurrent suites.
    (log_dir / executable.name).write_text(output)
    return label, status, time.monotonic() - started


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--partition", choices=("workspace", "modules"), required=True)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--shards", type=int, default=1)
    parser.add_argument("--shard", type=int, default=1, help="one-based shard number")
    parser.add_argument("--timeout", type=int, default=600, help="seconds per test binary")
    args = parser.parse_args(argv)
    if args.jobs < 1 or args.timeout < 1:
        parser.error("jobs and timeout must be positive")
    if args.shards < 1 or not 1 <= args.shard <= args.shards:
        parser.error("shard must be between 1 and shards")
    os.chdir(ROOT)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], text=True))
    expected = test_targets(metadata, args.partition)
    cargo = ["cargo", "test", "--locked", "--no-fail-fast", *selection(args.partition)]
    if args.partition == "modules":
        subprocess.run(["cargo", "build", "--locked", "-p", "p1-host", "--bin", "p1"], check=True)
    started = time.monotonic()
    # Cargo's stderr carries build diagnostics; JSON stdout is only artifact metadata.
    built = subprocess.run([*cargo, "--no-run", "--message-format=json"],
                           stdout=subprocess.PIPE, text=True)
    messages = [json.loads(line) for line in built.stdout.splitlines()]
    for message in messages:
        if message.get("reason") == "compiler-message":
            print(message["message"].get("rendered", ""), end="", file=sys.stderr)
    if built.returncode:
        return built.returncode
    targets = executables(messages, expected)
    inventory_count = len(targets)
    costs = json.loads((ROOT / "scripts/ci-test-costs.json").read_text())["seconds"]
    targets = shard_targets(targets, args.shard, args.shards, costs)
    build_seconds = time.monotonic() - started
    Path(metadata["target_directory"], "tmp").mkdir(exist_ok=True)
    timing_file = Path(metadata["target_directory"], "p1-test-timings.json")
    try:
        timings = json.loads(timing_file.read_text())
        if not isinstance(timings, dict) or not all(isinstance(v, (int, float)) for v in timings.values()):
            timings = {}
    except (OSError, ValueError):
        timings = {}
    targets.sort(key=lambda t: -timings.get(label_of(t), BASELINE_SECONDS.get(t[0][1], 1)))
    os.environ["P1_BIN"] = str(Path(metadata["target_directory"]) / "debug/p1")
    results = []
    totals = [0, 0, 0]
    with tempfile.TemporaryDirectory(prefix="p1-ci-tests-") as temp:
        log_dir = Path(temp)
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = {pool.submit(run_binary, target, log_dir, args.timeout): target for target in targets}
            for future in as_completed(futures):
                result = future.result()
                results.append(result)
                label, status, seconds = result
                output = (log_dir / futures[future][1].name).read_text()
                for counts in re.findall(r"test result:.*?(\d+) passed; (\d+) failed; (\d+) ignored;", output):
                    totals = [total + int(count) for total, count in zip(totals, counts)]
                print(f"{'FAIL' if status else 'ok'} {label}: {seconds:.2f}s (exit {status})", flush=True)
                if status:
                    print(output, flush=True)
    # rustdoc owns doctest discovery and execution. Run it even if a binary failed.
    doc_status = 0
    if args.shard == 1:
        docs = subprocess.run([*cargo, "--doc", "--", "--nocapture"],
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        print(docs.stdout)
        doc_status = docs.returncode or int(any(s in docs.stdout for s in SKIPS))
    feature_off = 0
    if args.partition == "workspace" and args.shard == 1:
        feature_off = subprocess.run([
            "timeout", "--foreground", "600", "cargo", "test", "--locked", "--no-fail-fast",
            "-p", "p1-host", "--no-default-features", "--test", "host", "without_delegation",
            "--", "--nocapture"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        print(feature_off.stdout)
        feature_off = feature_off.returncode or int(any(s in feature_off.stdout for s in SKIPS))
    elapsed = time.monotonic() - started
    timing_file.write_text(json.dumps({name: seconds for name, _, seconds in results}, indent=2) + "\n")
    summary = (f"### Tests: {args.partition}, shard {args.shard}/{args.shards}\n\n"
               f"{len(results)} of {inventory_count} executables; build {build_seconds:.2f}s; "
               f"build, tests and doctests {elapsed:.2f}s. "
               f"Binary cases: {totals[0]} passed, {totals[1]} failed, {totals[2]} ignored.\n\n"
               "| Target | Seconds | Exit |\n| --- | ---: | ---: |\n" +
               "".join(f"| {name} | {seconds:.2f} | {status} |\n"
                       for name, status, seconds in sorted(results, key=lambda r: -r[2])))
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as out:
            out.write(summary)
    print(f"tests: {args.partition}, shard {args.shard}/{args.shards}: "
          f"{len(results)} of {inventory_count} executables in {elapsed:.2f}s; "
          f"binary cases: {totals[0]} passed, {totals[1]} failed, {totals[2]} ignored", flush=True)
    return int(bool(doc_status or feature_off or any(r[1] for r in results)))


if __name__ == "__main__":
    raise SystemExit(main())
