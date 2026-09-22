#!/usr/bin/env python3
"""Run a multi-stage worker workflow on top of scripts/fanout.py.

    scripts/workflow.py DEFINITION.py --out DIR [--workspace DIR] [--env ENV]
                        [--max-parallel N] [--arg KEY=VALUE ...]

The shape is Claude Code's Workflow tool — agent(), parallel(), pipeline(), phase(), log() —
but every agent() is ONE fanout job (`"runner": "p1"`), so the workers are whatever the
environment names (DeepSeek on deepseek2). fanout keeps the pool, the memory bound and the
run reports; this layer adds only what a multi-stage run needs:

- structured output: the brief names one output file and its JSON Schema; the result is
  validated here (never trusted) together with an optional `check(obj) -> [errors]`;
- one repair round in the SAME session when validation fails, then the call yields None
  and the failure is recorded — never dropped silently;
- resume: an output that already exists and still validates is reused, so re-running the
  same definition with the same --out continues where it stopped;
- a journal (`journal.jsonl`) of every call and a `result.json` holding what the
  definition returned plus every failure.

A DEFINITION is a Python file with `META = {"name": ..., "description": ...}` and
`run(wf, args)`; `args` holds the --arg pairs. Whatever `run` returns is written to
`<out>/result.json`. The process exits when the workflow is finished (exit 0) or
aborted (exit 1): its exit is the completion signal, as with fanout.
"""
import argparse
import concurrent.futures
import importlib.util
import json
import os
import re
import subprocess
import sys
import threading

SCRIPTS_DIR = os.path.dirname(os.path.abspath(__file__))

SAFE_LABEL = re.compile(r"[^A-Za-z0-9._-]+")


class WorkflowError(Exception):
    """A definition that cannot run — reported, and the process exits 1."""


# ---- JSON Schema (the subset briefs use) -------------------------------------------------

def schema_errors(value, schema, path="$"):
    """Errors of `value` against a JSON Schema subset: type, enum, required, properties,
    additionalProperties (false only), items, minItems, minimum."""
    errors = []
    kind = schema.get("type")
    checks = {"object": lambda v: isinstance(v, dict),
              "array": lambda v: isinstance(v, list),
              "string": lambda v: isinstance(v, str),
              "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
              "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
              "boolean": lambda v: isinstance(v, bool)}
    if kind and not checks[kind](value):
        return [f"{path}: expected {kind}, got {type(value).__name__}"]
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{path}: {value!r} is not one of {schema['enum']}")
    if isinstance(value, dict):
        props = schema.get("properties", {})
        for key in schema.get("required", []):
            if key not in value:
                errors.append(f"{path}: missing required key {key!r}")
        for key, item in value.items():
            if key in props:
                errors += schema_errors(item, props[key], f"{path}.{key}")
            elif schema.get("additionalProperties") is False:
                errors.append(f"{path}: unexpected key {key!r}")
    if isinstance(value, list):
        if len(value) < schema.get("minItems", 0):
            errors.append(f"{path}: needs at least {schema['minItems']} items")
        if "items" in schema:
            for index, item in enumerate(value):
                errors += schema_errors(item, schema["items"], f"{path}[{index}]")
    if "minimum" in schema and isinstance(value, (int, float)) and value < schema["minimum"]:
        errors.append(f"{path}: {value} is below {schema['minimum']}")
    return errors


# ---- the workflow ---------------------------------------------------------------------------

OUTPUT_CONTRACT = """

---
# Output contract (workflow)

Your result is ONE JSON file, and nothing else counts:

    OUTPUT FILE: {path}

Write it with the shell tool (for example `cat > '{path}' <<'JSON'` … `JSON`), then check
it parses (`python3 -m json.tool '{path}'`). It must match this JSON Schema exactly — no
extra keys, the enum values exactly as written here (not as older text in the repository
spells them):

```json
{schema}
```

Create no other file. Then call finish with status "done" and, as verification, the command
that showed the file parses.
"""

REPAIR_PROMPT = """Your output file {path} was rejected by the workflow's validator:

{errors}

Fix exactly these problems and rewrite the whole file (same path, same schema). Do not
change content that was not rejected unless it is wrong. Then call finish again.
"""


class Workflow:
    """One run: an output directory, a workspace the workers run in, a default environment."""

    Error = WorkflowError  # a definition aborts the run with `raise wf.Error(reason)`

    def __init__(self, out_dir, workspace, env, max_parallel=6, fanout=None, sandbox=True,
                 repairs=1):
        self.out_dir = os.path.abspath(out_dir)
        self.workspace = os.path.abspath(workspace)
        self.env = env
        self.sandbox = sandbox
        self.repairs = repairs
        self.fanout = fanout or [sys.executable, os.path.join(SCRIPTS_DIR, "fanout.py")]
        self.slots = threading.Semaphore(max_parallel)
        self.max_parallel = max_parallel
        self.lock = threading.Lock()
        self.labels = set()
        self.failures = []
        self.current_phase = None
        for sub in ("outputs", "briefs", "jobs", "fanout"):
            os.makedirs(os.path.join(self.out_dir, sub), exist_ok=True)

    # -- narration -------------------------------------------------------------------------

    def phase(self, title):
        self.current_phase = title
        self.log(f"== {title}")

    def log(self, message):
        with self.lock:
            print(f"workflow: {message}", file=sys.stderr, flush=True)
            with open(os.path.join(self.out_dir, "workflow.log"), "a") as handle:
                handle.write(message + "\n")

    def _journal(self, record):
        with self.lock:
            with open(os.path.join(self.out_dir, "journal.jsonl"), "a") as handle:
                handle.write(json.dumps(record) + "\n")

    def fail(self, label, reason, **extra):
        """Record a failure the result must show (also usable by definitions)."""
        with self.lock:
            self.failures.append({"label": label, "reason": reason, **extra})
        self.log(f"FAILED {label}: {reason}")

    # -- agent -----------------------------------------------------------------------------

    def output_path(self, label):
        return os.path.join(self.out_dir, "outputs", f"{label}.json")

    def agent(self, label, brief, schema, env=None, check=None, phase=None, workspace=None,
              sandbox_write=(), after_run=None):
        """Run one worker; return its validated JSON object, or None (failure recorded).

        `check(obj)` returns extra error strings (evidence checks). `workspace` overrides
        the run's workspace for this call; `after_run(label)` returns an error string when
        the call must be voided after the worker ran (e.g. it wrote to the workspace)."""
        label = SAFE_LABEL.sub("-", label)
        with self.lock:
            if label in self.labels:
                raise WorkflowError(f"duplicate agent label {label}")
            self.labels.add(label)
        phase = phase or self.current_phase
        path = self.output_path(label)
        cached = self._validated(path, schema, check)
        if cached is not None and not cached[1]:
            self._journal({"label": label, "phase": phase, "cached": True})
            return cached[0]

        brief_path = os.path.join(self.out_dir, "briefs", f"{label}.md")
        with open(brief_path, "w") as handle:
            handle.write(brief.rstrip() + OUTPUT_CONTRACT.format(
                path=path, schema=json.dumps(schema, indent=2)))
        if os.path.exists(path):
            os.remove(path)
        job = {"label": label, "runner": "p1", "env": env or self.env,
               "dir": workspace or self.workspace, "brief_file": brief_path,
               "sandbox": self.sandbox}
        if self.sandbox:
            job["sandbox_write"] = [os.path.dirname(path), *sandbox_write]
        record = self._run_job(job)
        attempt = 0
        while True:
            voided = after_run(label) if after_run else None
            if voided:
                self.fail(label, voided, run_dir=record.get("run_dir"))
                self._journal({"label": label, "phase": phase, "voided": voided})
                return None
            result = self._validated(path, schema, check)
            errors = ["no output file was written"] if result is None else result[1]
            if not errors:
                self._journal({"label": label, "phase": phase, "run_dir": record.get("run_dir"),
                               "repairs": attempt, "usage": record.get("usage")})
                return result[0]
            session = os.path.join(record.get("run_dir") or "", "session.jsonl")
            if attempt >= self.repairs or not os.path.isfile(session):
                self.fail(label, "invalid output", errors=errors[:20],
                          run_dir=record.get("run_dir"))
                self._journal({"label": label, "phase": phase, "invalid": errors[:20]})
                return None
            attempt += 1
            self.log(f"{label}: {len(errors)} validation error(s), repair round {attempt}")
            prompt = os.path.join(self.out_dir, "briefs", f"{label}-repair{attempt}.md")
            with open(prompt, "w") as handle:
                handle.write(REPAIR_PROMPT.format(
                    path=path, errors="\n".join(f"- {e}" for e in errors[:40])))
            repair = dict(job, label=f"{label}-repair{attempt}", session=session,
                          prompt_file=prompt)
            repair.pop("brief_file")
            record = self._run_job(repair)

    def _validated(self, path, schema, check):
        """(obj, errors) for an output file, or None when there is no readable file."""
        try:
            with open(path) as handle:
                obj = json.load(handle)
        except FileNotFoundError:
            return None
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            return None, [f"not valid JSON: {error}"]
        errors = schema_errors(obj, schema)
        if not errors and check:
            errors = list(check(obj))
        return obj, errors

    def _run_job(self, job):
        """One fanout batch holding one job; returns fanout's record for it."""
        jobs_path = os.path.join(self.out_dir, "jobs", f"{job['label']}.json")
        with open(jobs_path, "w") as handle:
            json.dump([job], handle, indent=2)
        with self.slots:
            self.log(f"start {job['label']} ({job['env']})")
            done = subprocess.run(
                [*self.fanout, jobs_path, "--max-parallel", str(self.max_parallel),
                 "--out-dir", os.path.join(self.out_dir, "fanout")],
                capture_output=True, text=True)
        try:
            record = json.loads(done.stdout)[0]
        except (json.JSONDecodeError, IndexError, KeyError):
            record = {"label": job["label"], "error": done.stderr[-2000:]}
        self.log(f"end {job['label']}: {record.get('outcome', record.get('error', '?'))}")
        return record

    # -- composition -----------------------------------------------------------------------

    def parallel(self, thunks):
        """Run thunks concurrently; a barrier. A thunk that raises yields None."""
        thunks = list(thunks)
        if not thunks:
            return []
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(thunks)) as pool:
            futures = [pool.submit(thunk) for thunk in thunks]
            return [self._value(future) for future in futures]

    def pipeline(self, items, *stages):
        """Each item runs through every stage on its own — no barrier between stages. A
        stage gets (previous_result, item, index); a stage that raises, or returns None,
        drops that item to None."""
        def chain(item, index):
            value = item
            for stage in stages:
                value = stage(value, item, index)
                if value is None:
                    return None
            return value
        return self.parallel([lambda item=item, index=index: chain(item, index)
                              for index, item in enumerate(items)])

    def _value(self, future):
        try:
            return future.result()
        except WorkflowError:
            raise
        except Exception as error:  # a definition bug in one item must not sink the batch
            self.fail("(thunk)", f"{type(error).__name__}: {error}")
            return None


def load_definition(path):
    spec = importlib.util.spec_from_file_location("workflow_definition", path)
    if spec is None or spec.loader is None:
        raise WorkflowError(f"cannot load definition {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    if not isinstance(getattr(module, "META", None), dict) or not callable(getattr(module, "run", None)):
        raise WorkflowError(f"{path}: a definition needs META (dict) and run(wf, args)")
    return module


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("definition")
    ap.add_argument("--out", required=True, help="run directory (re-use it to resume)")
    ap.add_argument("--workspace", default=os.getcwd())
    ap.add_argument("--env", default="deepseek2")
    ap.add_argument("--max-parallel", type=int, default=6)
    ap.add_argument("--no-sandbox", action="store_true")
    ap.add_argument("--arg", action="append", default=[], metavar="KEY=VALUE")
    a = ap.parse_args(argv)
    try:
        args = dict(pair.split("=", 1) for pair in a.arg)
    except ValueError:
        print("workflow: --arg takes KEY=VALUE", file=sys.stderr)
        return 1
    try:
        definition = load_definition(a.definition)
        wf = Workflow(a.out, a.workspace, a.env, a.max_parallel, sandbox=not a.no_sandbox)
        wf.log(f"run {definition.META['name']}: {definition.META.get('description', '')}")
        returned = definition.run(wf, args)
    except WorkflowError as error:
        print(f"workflow: {error}", file=sys.stderr)
        return 1
    result = {"name": definition.META["name"], "result": returned, "failures": wf.failures}
    with open(os.path.join(wf.out_dir, "result.json"), "w") as handle:
        json.dump(result, handle, indent=2)
    print(json.dumps({"result_file": os.path.join(wf.out_dir, "result.json"),
                      "failures": len(wf.failures)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
