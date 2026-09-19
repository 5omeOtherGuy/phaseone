#!/usr/bin/env python3
"""Fan a list of briefs out to worker models directly — no relay model in between.

    scripts/fanout.py jobs.json [--max-parallel N] [--min-free-mb MB] [--out-dir DIR]

jobs.json is a list of jobs:
    {"label": "core-tests", "profile": "glm53", "effort": "high",
     "dir": "/abs/worktree", "brief_file": "/abs/brief.md"}
  resume / repair round in the SAME worker session:
    {"label": "core-fix", "profile": "deepseek", "effort": "high",
     "dir": "/abs/worktree", "session": "<id>", "prompt_file": "/abs/defects.md"}
  optional "after": ["label", ...] — start only when those jobs exited with 0.

Every job is one `pi-worker` process (found on PATH). The script blocks until all
jobs have exited, then prints one JSON summary (model actually used, exit, session,
cost, run dir, output file per job). Run it as ONE background task: its exit is the
completion signal for the whole batch. No time limits, no retries, no judging —
accepting a result stays the lead's job.

Compiles are serialised machine-wide by cargo's lock on the shared target dir
(scripts/local-cargo-config.sh). A worker process itself costs ~120-190 MB resident
(measured), so the pool is bounded twice: at most --max-parallel live workers
(default 6) and no new worker starts while MemAvailable is below --min-free-mb
(default 1500). Both bounds count EVERY pi-worker on the machine, not only this
batch's; a job starts regardless only when no worker is running anywhere (so a
batch can never wait forever on memory that nothing will free). The job list may be much longer than
the pool — queue 20 jobs, run 6 at a time.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time

TRAILER_FIELDS = {
    "model_used": r"model_used: (.+?)\s{2,}effort:",
    "exit": r"\bexit: (-?\d+)",
    "session": r"session: (\S+)",
    "tool_calls": r"tool_calls: (\d+)",
    "elapsed_s": r"elapsed_s: (\d+)",
    "cost_usd": r"cost_usd: ([\d.]+)",
    "run_dir": r"run_dir: (\S+)",
}


def parse_trailer(text):
    tail = text.rsplit("--- pi-worker trailer ---", 1)
    if len(tail) != 2:
        return {}
    found = {}
    for key, pattern in TRAILER_FIELDS.items():
        match = re.search(pattern, tail[1])
        if match:
            found[key] = match.group(1).strip()
    warnings = [line for line in tail[1].splitlines() if re.match(r"(WARNING|POLICY VIOLATION|CUT OFF)", line)]
    if warnings:
        found["warnings"] = warnings
    return found


def mem_available_mb():
    for line in open("/proc/meminfo"):
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) // 1024
    return 0


def workers_alive():
    """pi-worker processes on the whole machine (other batches and sessions included)."""
    out = subprocess.run(["pgrep", "-fc", "scripts/pi-worker "], capture_output=True, text=True).stdout
    return int(out.strip() or 0)


def command_for(job, worker):
    cmd = [worker, job["profile"], "--dir", job["dir"], "--effort", job.get("effort", "high")]
    if job.get("session"):
        cmd += ["--session", job["session"], "--prompt", open(job["prompt_file"]).read()]
    else:
        cmd += ["--brief-file", job["brief_file"]]
    return cmd


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("jobs")
    ap.add_argument("--max-parallel", type=int, default=6)
    ap.add_argument("--min-free-mb", type=int, default=1500)
    ap.add_argument("--out-dir", help="where <label>.out files go (default: next to jobs.json)")
    a = ap.parse_args()

    worker = shutil.which("pi-worker")
    if not worker:
        sys.exit("fanout: pi-worker not found on PATH")
    jobs = json.load(open(a.jobs))
    labels = [j["label"] for j in jobs]
    if len(set(labels)) != len(labels):
        sys.exit("fanout: duplicate job labels")
    for j in jobs:
        for dep in j.get("after", []):
            if dep not in labels:
                sys.exit(f"fanout: job {j['label']} waits for unknown job {dep}")
        if not os.path.isdir(j["dir"]):
            sys.exit(f"fanout: job {j['label']}: no such workspace {j['dir']}")
        if not os.path.isfile(j.get("prompt_file") if j.get("session") else j.get("brief_file", "")):
            sys.exit(f"fanout: job {j['label']}: brief/prompt file missing")
    out_dir = a.out_dir or os.path.dirname(os.path.abspath(a.jobs))
    os.makedirs(out_dir, exist_ok=True)

    pending = {j["label"]: j for j in jobs}
    running, results = {}, {}
    while pending or running:
        for label, job in list(pending.items()):
            deps = job.get("after", [])
            failed = [d for d in deps if d in results and results[d].get("exit") != "0"]
            if failed:
                results[label] = {"label": label, "profile": job["profile"], "skipped": f"dependency failed: {failed}"}
                del pending[label]
            elif all(d in results for d in deps) and (
                    workers_alive() == 0
                    or (workers_alive() < a.max_parallel and mem_available_mb() >= a.min_free_mb)):
                out_path = os.path.join(out_dir, f"{label}.out")
                out = open(out_path, "w")
                proc = subprocess.Popen(command_for(job, worker), stdin=subprocess.DEVNULL, stdout=out,
                                        stderr=subprocess.STDOUT, start_new_session=True)
                running[label] = (proc, out, out_path, job, time.time())
                del pending[label]
                print(f"fanout: started {label} ({job['profile']}) pid {proc.pid}", file=sys.stderr, flush=True)
        for label, (proc, out, out_path, job, started) in list(running.items()):
            if proc.poll() is None:
                continue
            out.close()
            record = {"label": label, "profile": job["profile"], "effort": job.get("effort", "high"),
                      "workspace": job["dir"], "out_file": out_path, "process_exit": proc.returncode,
                      "wall_s": round(time.time() - started)}
            record.update(parse_trailer(open(out_path, errors="replace").read()))
            if "run_dir" not in record:
                record["error"] = "no pi-worker trailer — read out_file"
            results[label] = record
            del running[label]
            print(f"fanout: finished {label} exit {record.get('exit', proc.returncode)}", file=sys.stderr, flush=True)
        if running or pending:
            time.sleep(5 if pending else 2)

    print(json.dumps([results[label] for label in labels], indent=2))
    sys.exit(0 if all(r.get("exit") == "0" for r in results.values()) else 1)


if __name__ == "__main__":
    main()
