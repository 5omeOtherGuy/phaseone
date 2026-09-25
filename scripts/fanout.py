#!/usr/bin/env python3
"""Fan a list of briefs out to worker models directly — no relay model in between.

    scripts/fanout.py jobs.json [--max-parallel N] [--min-free-mb MB] [--out-dir DIR]

jobs.json is a list of jobs. A `pi-worker` job (the default runner) is:
    {"label": "core-tests", "profile": "glm53", "effort": "high",
     "dir": "/abs/worktree", "brief_file": "/abs/brief.md"}
  resume / repair round in the SAME worker session:
    {"label": "core-fix", "profile": "deepseek", "effort": "high",
     "dir": "/abs/worktree", "session": "<id>", "prompt_file": "/abs/defects.md"}

A `p1` job runs the work with p1 ITSELF — with full access by default, like p1 (ADR-0038);
"sandbox": true confines the agent's shell to the workspace — and keeps the session journal
and an evidence record (scripts/run-report.py) in a run directory:
    {"label": "core-tests", "runner": "p1", "env": "claude",
     "dir": "/abs/worktree", "brief_file": "/abs/brief.md"}
  repair round — resume an earlier run's journal:
    {"label": "core-fix", "runner": "p1", "env": "claude",
     "dir": "/abs/worktree", "session": "<run>/session.jsonl",
     "prompt_file": "/abs/defects.md"}
  optional "model": "E/P[:effort]" reaches `p1 --model` (the environment's own profile
  otherwise); optional "sandbox_write": ["/abs/path", ...] and "max_continuations": N reach the
  matching p1 flags ("sandbox_write" only with "sandbox": true); `profile`/`effort` are
  unused by this runner. When a SANDBOXED job's `dir` is a git
  WORKTREE (its `--git-common-dir` is outside `dir`) the common directory is passed as
  `--sandbox-read` so the agent can inspect (never commit) the git metadata that lives in
  the main checkout.

Every job also accepts "after": ["label", ...] — start only when those jobs finished
successfully (exit 0 / outcome done).

A `p1` job's binary is `$P1_BIN`, else `p1` on `PATH` (an installed p1), else the debug
build of a sibling checkout (`../phaseone-target/debug/p1`).

The script blocks until all jobs have exited, then prints one JSON summary (runner,
exit, outcome, run dir, cost and counters per job). Run it as ONE background task: its
exit is the completion signal for the whole batch. No time limits, no retries, no
judging — accepting a result stays the lead's job.

Compiles are serialised machine-wide by cargo's lock on the shared target dir
(scripts/local-cargo-config.sh). The pool is bounded twice: at most --max-parallel live
workers (default 6) and no new worker starts while MemAvailable is below --min-free-mb
(default 1500). Both bounds count EVERY pi-worker AND every headless p1 worker on the
machine, not only this batch's; a job starts regardless only when no worker is running
anywhere (so a batch can never wait forever on memory that nothing will free). The job
list may be much longer than the pool — queue 20 jobs, run 6 at a time.
"""
import argparse
import hashlib
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

# poll cadence while jobs are running; tests shorten this.
POLL_RUNNING_S = 2
POLL_QUEUED_S = 5


class JobError(Exception):
    """A jobs file that cannot be dispatched — reported before anything starts."""


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


def mem_available_mb(reader=None):
    """Return available memory in MiB, or None when /proc/meminfo cannot be trusted."""
    if reader is None:
        reader = open
    try:
        with reader("/proc/meminfo") as handle:
            for line in handle:
                fields = line.split()
                if not fields or fields[0] != "MemAvailable:":
                    continue
                if len(fields) < 2:
                    return None
                try:
                    value = int(fields[1])
                except ValueError:
                    return None
                return value // 1024 if value >= 0 else None
    except (OSError, TypeError):
        return None
    return None


def is_pi_worker(argv):
    """A real pi-worker process: the script itself (`python3 …/pi-worker <profile> …` or
    `pi-worker <profile> …`), never a wrapper whose command LINE merely mentions it — a
    `bash -c "… pi-worker …"` loop or a `usage-meter wrap … -- pi-worker …` parent. Counting
    those once made two real workers look like six and filled the pool."""
    if not argv:
        return False
    head = os.path.basename(argv[0])
    if head == "pi-worker":
        return True
    return head.startswith("python") and len(argv) > 1 and os.path.basename(argv[1]) == "pi-worker"


# p1 subcommands (crates/p1-host/src/cli.rs): first-token words and flags that are never
# an agent run — `p1 models`, `p1 login …`, `p1 --help` and the like.
P1_SUBCOMMANDS = {"env", "models", "usage", "login", "logout", "workflow", "help",
                  "--help", "-h", "--version", "-V"}

# p1 run-mode flags that swallow the NEXT token as their value (cli.rs `take_value`).
P1_VALUE_FLAGS = {
    "--env", "--model", "--effort", "--models", "--workspace", "--session",
    "--instructions", "--skills", "--sandbox", "--sandbox-write", "--sandbox-read",
    "--env-pass", "--max-continuations", "--provider-retries", "--max-idle-summaries",
}

# p1 run-mode flags that take no value.
P1_BOOL_FLAGS = {"--resume", "--ask", "--yes"}


def is_p1_agent(argv):
    """A HEADLESS p1 worker, decided from argv alone (/proc/<pid>/cmdline — environ is
    never read): basename `p1` or `p1-*`, no `--tui`, and a positional prompt. The accepted option
    shape is the same run grammar used by p1_command; unsupported prompt-file flags do not
    count because p1 rejects them. An interactive session (`p1`, `p1 --env claude`, or
    anything `--tui`) and meta subcommands (`p1 models`, `p1 login …`) never count."""
    # A renamed build (P1_BIN=…/p1-hotfix2-<sha>) is still p1.
    name = os.path.basename(argv[0]) if argv else ""
    if name != "p1" and not name.startswith("p1-"):
        return False
    args = argv[1:]
    # `p1 workflow run FILE` runs unattended (cli.rs `Options::is_headless`); its steps run
    # in that one process, so the process counts once.
    if args[:2] == ["workflow", "run"]:
        return True
    if not args or args[0] in P1_SUBCOMMANDS or "--tui" in args:
        return False
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "--":
            # `--` ends option parsing; everything after it is the prompt (cli.rs).
            return index + 1 < len(args)
        if arg in P1_VALUE_FLAGS:
            index += 2
            continue
        if arg in P1_BOOL_FLAGS:
            index += 1
            continue
        if arg.startswith("-") and arg != "-":
            return False
        return True  # a bare word: the positional prompt of a headless run
    return False


def processes():
    """argv of every process on the machine (a /proc scan, no psutil)."""
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as handle:
                argv = [part.decode("utf-8", "replace") for part in handle.read().split(b"\0") if part]
        except OSError:
            continue
        yield argv


def pi_workers_alive():
    """pi-worker processes on the whole machine (other batches and sessions included)."""
    return sum(1 for argv in processes() if is_pi_worker(argv))


def p1_agents_alive():
    """Headless p1 workers machine-wide."""
    return sum(1 for argv in processes() if is_p1_agent(argv))


def workers_alive():
    """Both kinds of worker, everywhere on the machine."""
    return pi_workers_alive() + p1_agents_alive()


def runner_of(job):
    return job.get("runner", "pi-worker")


def read_file(path, errors="strict"):
    with open(path, encoding="utf-8", errors=errors) as handle:
        return handle.read()


def command_for(job, worker):
    """The pi-worker command line (unchanged by the p1 runner)."""
    cmd = [worker, job["profile"], "--dir", job["dir"], "--effort", job.get("effort", "high")]
    if job.get("session"):
        cmd += ["--session", job["session"], "--prompt", read_file(job["prompt_file"])]
    else:
        cmd += ["--brief-file", job["brief_file"]]
    return cmd


def outcome_for_exit(code):
    """p1's process contract -> one word. Anything unmapped is a plain failure."""
    return {0: "done", 3: "blocked", 4: "stalled", 130: "cancelled"}.get(code, "failed")


def default_p1_binary():
    """The debug binary of a sibling checkout — the last resort, not the first choice."""
    scripts_dir = os.path.dirname(os.path.abspath(__file__))
    repo_parent = os.path.dirname(os.path.dirname(scripts_dir))
    return os.path.join(repo_parent, "phaseone-target", "debug", "p1")


def p1_binary():
    """The p1 to run: `$P1_BIN`, else `p1` on PATH, else the sibling debug build.

    An installed p1 (`<prefix>/bin/p1`, ADR-0065) is found through PATH, so a machine
    that installed the release needs no checkout and no `P1_BIN`.
    """
    override = os.environ.get("P1_BIN")
    if override:
        if not (os.path.isfile(override) and os.access(override, os.X_OK)):
            raise JobError(f"fanout: no p1 binary at {override} — run: cargo build -p p1-host")
        name = os.path.basename(override)
        # The pool recognises its workers by argv[0] alone (is_p1_agent).
        if name != "p1" and not name.startswith("p1-"):
            raise JobError(f"fanout: P1_BIN must be named p1 or p1-* so the pool can count its "
                           f"workers, not {name} — point it at a symlink with such a name")
        return override
    on_path = shutil.which("p1")
    if on_path:
        return on_path
    path = default_p1_binary()
    if not (os.path.isfile(path) and os.access(path, os.X_OK)):
        raise JobError(
            f"fanout: no p1 binary at {path} — run: cargo build -p p1-host, "
            "or install p1 with scripts/install.sh"
        )
    return path


def build_locks_dir():
    return f"/tmp/p1-build-locks-{os.getuid()}"


def git_common_dir(workdir):
    """The git common dir of `workdir`, absolute, or None when it is not a repo.

    A worktree's common dir is the main checkout's `.git/`; a plain clone's is its own
    `.git`, which the sandbox already makes visible inside the workspace.
    """
    try:
        done = subprocess.run(["git", "-C", workdir, "rev-parse", "--git-common-dir"],
                              capture_output=True, text=True)
    except OSError:
        return None
    if done.returncode != 0:
        return None
    path = done.stdout.strip()
    if not path:
        return None
    if not os.path.isabs(path):
        path = os.path.join(workdir, path)
    return os.path.realpath(path)


def sandbox_read_paths(workdir):
    """The `--sandbox-read` paths for a job dir: the git common dir when `workdir` is a
    worktree (the common dir is OUTSIDE the dir), so `git status`/`git diff` work.
    Read-only on purpose: the agent may inspect, never commit.
    """
    common = git_common_dir(workdir)
    if common is None:
        return []
    root = os.path.realpath(workdir)
    if common == root or common.startswith(root + os.sep):
        return []
    return [common]


def p1_command(job, binary, session_path, brief, locks_dir, readable=()):
    """The exact p1 argv for one job (pure; the resume flag follows the session and the
    read-only paths are injected by the caller)."""
    cmd = [binary, "--env", job["env"], "--workspace", job["dir"], "--session", session_path]
    # Owner policy 2026-09-23 (model-cards replacement trial): a job may pick the model inside
    # its environment, `E/P[:effort]` exactly as `p1 --model` takes it.
    if job.get("model"):
        cmd += ["--model", job["model"]]
    if job.get("session"):
        cmd.append("--resume")
    cmd.append("--yes")
    # Owner decision 2026-09-20: full access is the default here as in p1 itself (ADR-0038);
    # a job opts INTO the workspace sandbox with "sandbox": true.
    if job.get("sandbox", False):
        cmd += ["--sandbox", "workspace",
                "--sandbox-write", os.path.expanduser("~/.cargo/registry"),
                "--sandbox-write", os.path.expanduser("~/.cargo/git"),
                "--sandbox-write", locks_dir]
        for path in job.get("sandbox_write", []):
            cmd += ["--sandbox-write", path]
        for path in readable:
            cmd += ["--sandbox-read", path]
    if job.get("max_continuations") is not None:
        cmd += ["--max-continuations", str(job["max_continuations"])]
    cmd.append(brief)
    return cmd


def unique_path(directory, filename):
    """`filename` in `directory`, suffixed -2, -3… if it is already taken."""
    stem, ext = os.path.splitext(filename)
    candidate = os.path.join(directory, filename)
    counter = 2
    while os.path.exists(candidate):
        candidate = os.path.join(directory, f"{stem}-{counter}{ext}")
        counter += 1
    return candidate


def p1_run_dir(job, out_dir):
    """A fresh run gets its own stamped directory; a resume stays where its journal is."""
    if job.get("session"):
        return os.path.dirname(os.path.abspath(job["session"]))
    runs_dir = os.path.join(out_dir, "runs")
    os.makedirs(runs_dir, exist_ok=True)
    return unique_path(runs_dir, f"{job['label']}-{time.strftime('%Y%m%d-%H%M%S')}")


def validate_jobs(jobs):
    """Refuse an undispatchable batch before any process starts."""
    labels = [job["label"] for job in jobs]
    if len(set(labels)) != len(labels):
        raise JobError("fanout: duplicate job labels")
    for job in jobs:
        label = job["label"]
        kind = runner_of(job)
        if kind not in ("pi-worker", "p1"):
            raise JobError(f"fanout: job {label}: unknown runner {kind!r}")
        for dep in job.get("after", []):
            if dep not in labels:
                raise JobError(f"fanout: job {label} waits for unknown job {dep}")
        if not os.path.isdir(job["dir"]):
            raise JobError(f"fanout: job {label}: no such workspace {job['dir']}")
        if job.get("session") and not job.get("prompt_file"):
            raise JobError(f"fanout: job {label}: session requires prompt_file")
        if kind == "pi-worker":
            needed = job.get("prompt_file") if job.get("session") else job.get("brief_file", "")
            if not os.path.isfile(needed or ""):
                raise JobError(f"fanout: job {label}: brief/prompt file missing")
            continue
        if not job.get("env"):
            raise JobError(f"fanout: job {label}: p1 runner needs env")
        if job.get("session"):
            if not os.path.isfile(job["session"]):
                raise JobError(f"fanout: job {label}: no such session {job['session']}")
            if not os.path.isfile(job["prompt_file"]):
                raise JobError(f"fanout: job {label}: prompt file missing: {job['prompt_file']}")
        elif not os.path.isfile(job.get("brief_file") or ""):
            raise JobError(f"fanout: job {label}: brief file missing: {job.get('brief_file')}")
        # An empty prompt would be an empty argv element, which /proc cmdline parsing drops, so
        # the pool could not count the worker it started.
        prompt_path = job["prompt_file"] if job.get("session") else job["brief_file"]
        with open(prompt_path, encoding="utf-8", errors="replace") as handle:
            if not handle.read().strip():
                raise JobError(f"fanout: job {label}: prompt is empty: {prompt_path}")
        if not isinstance(job.get("sandbox", False), bool):
            raise JobError(f"fanout: job {label}: sandbox must be true or false")
        if job.get("sandbox_write") and not job.get("sandbox", False):
            raise JobError(f"fanout: job {label}: sandbox_write needs \"sandbox\": true")
        for path in job.get("sandbox_write", []):
            if not os.path.isabs(path):
                raise JobError(f"fanout: job {label}: sandbox_write must be absolute: {path}")
        if "max_continuations" in job:
            value = job["max_continuations"]
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise JobError(f"fanout: job {label}: max_continuations must be a non-negative integer")


def launch(job, worker, binary, out_dir):
    """Start one job's process; returns the state the poll loop needs."""
    started = time.time()
    if runner_of(job) == "p1":
        run_dir = p1_run_dir(job, out_dir)
        os.makedirs(run_dir, exist_ok=True)
        session = os.path.abspath(job["session"]) if job.get("session") else os.path.join(run_dir, "session.jsonl")
        brief = read_file(job["prompt_file"] if job.get("session") else job["brief_file"])
        stdout_path = unique_path(run_dir, "stdout.txt")
        stderr_path = unique_path(run_dir, "stderr.txt")
        with open(unique_path(run_dir, "task.txt"), "w", encoding="utf-8") as handle:
            handle.write(brief)
        stdout = open(stdout_path, "w")
        stderr = open(stderr_path, "w")
        proc = subprocess.Popen(p1_command(job, binary, session, brief, build_locks_dir(),
                                           sandbox_read_paths(job["dir"])),
                                stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                                start_new_session=True)
        return {"proc": proc, "job": job, "started": started, "identity": build_identity(binary),
                "stdout": stdout, "stderr": stderr,
                "stdout_path": stdout_path, "stderr_path": stderr_path,
                "run_dir": run_dir, "session": session}
    out_path = os.path.join(out_dir, f"{job['label']}.out")
    out = open(out_path, "w")
    proc = subprocess.Popen(command_for(job, worker), stdin=subprocess.DEVNULL, stdout=out,
                            stderr=subprocess.STDOUT, start_new_session=True)
    return {"proc": proc, "job": job, "started": started, "stdout": out, "stderr": None,
            "out_path": out_path}


def build_identity(binary):
    """(repository HEAD, sha256 of the p1 binary) — either is None when it cannot be read."""
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    head = subprocess.run(["git", "-C", repo, "rev-parse", "--short", "HEAD"],
                          capture_output=True, text=True)
    digest = None
    try:
        with open(binary, "rb") as handle:
            digest = hashlib.file_digest(handle, "sha256").hexdigest()
    except OSError:
        pass
    return (head.stdout.strip() or None) if head.returncode == 0 else None, digest


def run_report(session, run_dir, label, elapsed, exit_code, identity=(None, None)):
    """scripts/run-report.py once per p1 job; its JSON lands in the run directory.

    Acceptance is the lead's call — run-report defaults it to `unknown`.
    """
    script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "run-report.py")
    done = subprocess.run([sys.executable, script, session, "--label", label,
                           "--elapsed", str(elapsed), "--exit-code", str(exit_code)]
                          + [part for flag, value in zip(("--harness-head", "--binary-sha256"), identity)
                             if value for part in (flag, value)],
                          capture_output=True, text=True)
    report_path = os.path.join(run_dir, "report.json")
    with open(report_path, "w", encoding="utf-8") as handle:
        handle.write(done.stdout)
    if done.returncode != 0:
        return {}, report_path, done.stderr.strip()
    return json.loads(done.stdout), report_path, None


def p1_record(state, process_exit):
    job = state["job"]
    wall_s = round(time.time() - state["started"])
    report, _, error = run_report(state["session"], state["run_dir"], job["label"], wall_s, process_exit,
                                  state.get("identity", (None, None)))
    record = {
        "runner": "p1",
        "label": job["label"],
        "env": job["env"],
        "workspace": job["dir"],
        "run_dir": state["run_dir"],
        "process_exit": process_exit,
        "outcome": outcome_for_exit(process_exit),
        "wall_s": wall_s,
        "stdout_file": state["stdout_path"],
        "stderr_file": state["stderr_path"],
        "requests": report.get("requests"),
        "tool_calls": report.get("tool_calls"),
        "tool_calls_not_ok": report.get("tool_calls_not_ok"),
        "shell_exits": report.get("shell_exits"),
        "input_total_with_workers": report.get("input_total_with_workers", report.get("input_total")),
        "cache_read_share": report.get("cache_read_share"),
        "usage": report.get("usage"),
    }
    if error:
        record["report_error"] = error
    return record


def record_ok(record):
    if record.get("runner") == "p1":
        return record.get("outcome") == "done"
    return record.get("exit") == "0"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("jobs")
    ap.add_argument("--max-parallel", type=int, default=6)
    ap.add_argument("--min-free-mb", type=int, default=1500)
    ap.add_argument("--out-dir", help="where <label>.out files go (default: next to jobs.json)")
    a = ap.parse_args(argv)

    jobs = json.load(open(a.jobs))
    try:
        validate_jobs(jobs)
        binary = p1_binary() if any(runner_of(job) == "p1" for job in jobs) else None
    except JobError as error:
        print(error, file=sys.stderr)
        return 1
    worker = None
    if any(runner_of(job) == "pi-worker" for job in jobs):
        worker = shutil.which("pi-worker")
        if not worker:
            print("fanout: pi-worker not found on PATH", file=sys.stderr)
            return 1
    out_dir = a.out_dir or os.path.dirname(os.path.abspath(a.jobs))
    os.makedirs(out_dir, exist_ok=True)

    labels = [job["label"] for job in jobs]
    pending = {job["label"]: job for job in jobs}
    running, results = {}, {}
    last_wait_reason = None
    memory_unknown_reported = False
    while pending or running:
        waiting_reason = None
        waiting_snapshot = None
        for label, job in list(pending.items()):
            deps = job.get("after", [])
            failed = [d for d in deps if d in results and not record_ok(results[d])]
            if failed:
                results[label] = {"label": label, "profile": job.get("profile"),
                                  "skipped": f"dependency failed: {failed}"}
                del pending[label]
                continue
            if not all(d in results for d in deps):
                continue
            alive = workers_alive()
            available = mem_available_mb()
            if available is None and not memory_unknown_reported:
                print("fanout: MemAvailable unknown — memory floor not enforced",
                      file=sys.stderr, flush=True)
                memory_unknown_reported = True
            if alive != 0 and (alive >= a.max_parallel
                               or (available is not None and available < a.min_free_mb)):
                reason = "pool" if alive >= a.max_parallel else "memory"
                waiting_reason = waiting_reason or reason
                waiting_snapshot = waiting_snapshot or (alive, available)
                continue
            state = launch(job, worker, binary, out_dir)
            running[label] = state
            del pending[label]
            if runner_of(job) == "p1":
                print(f"fanout: started {label} (p1 {job['env']}) pid {state['proc'].pid}",
                      file=sys.stderr, flush=True)
            else:
                print(f"fanout: started {label} ({job['profile']}) pid {state['proc'].pid}",
                      file=sys.stderr, flush=True)
        if waiting_reason is not None and waiting_reason != last_wait_reason:
            alive, available = waiting_snapshot
            print(f"fanout: waiting — {alive} workers alive ({a.max_parallel} max), "
                  f"MemAvailable {'unknown' if available is None else f'{available} MB'}",
                  file=sys.stderr, flush=True)
        last_wait_reason = waiting_reason
        for label, state in list(running.items()):
            proc = state["proc"]
            if proc.poll() is None:
                continue
            state["stdout"].close()
            if state["stderr"]:
                state["stderr"].close()
            if runner_of(state["job"]) == "p1":
                record = p1_record(state, proc.returncode)
                print(f"fanout: finished {label} exit {proc.returncode} outcome {record['outcome']}",
                      file=sys.stderr, flush=True)
            else:
                record = {"label": label, "profile": state["job"]["profile"],
                          "effort": state["job"].get("effort", "high"),
                          "workspace": state["job"]["dir"], "out_file": state["out_path"],
                          "process_exit": proc.returncode,
                          "wall_s": round(time.time() - state["started"])}
                record.update(parse_trailer(read_file(state["out_path"], errors="replace")))
                if "run_dir" not in record:
                    record["error"] = "no pi-worker trailer — read out_file"
                print(f"fanout: finished {label} exit {record.get('exit', proc.returncode)}",
                      file=sys.stderr, flush=True)
            results[label] = record
            del running[label]
        if running or pending:
            time.sleep(POLL_QUEUED_S if pending else POLL_RUNNING_S)

    print(json.dumps([results[label] for label in labels], indent=2))
    return 0 if all(record_ok(r) for r in results.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
