"""Modularity audit of p1 — a scripts/workflow.py definition.

    scripts/workflow.py scripts/audits/modularity.py --out ../phaseone-briefs/modularity-audit/out \
        --env deepseek2-audit --arg repo=$PWD [--arg sha=<commit>] [--arg verify_cap=30] \
        [--arg critic_rounds=2] [--arg only=<unit>,<unit>]   # `only`: a smoke run

Stages (design: ../phaseone-briefs/modularity-audit/workflow.md, reviewed with pane %39):
  Scout   — deterministic facts and scripted findings; the unit list is derived from the
            tree and validated (every path exists, every unit within the line budget, every
            crate covered), else the run aborts.
  Find    — one worker per unit × lens, every unit file read in full (checked in the
            session journal, not taken from the worker's word).
  Verify  — two refuters per finding (rule lens, code lens) on deepseek2-audit-max; upheld
            only if both uphold; a split high-severity finding gets a third vote.
  Critic  — coverage gaps become new Find units; at most two rounds, stops when dry.
The auditor environments live in scripts/audits/environments; each run links them with the
repository's profiles and routes into <out>/p1-share and checks they resolve before any job.
Every job runs in its own detached worktree at the pinned commit; a job that leaves its
worktree dirty is voided. Every finding ends in exactly one bucket: confirmed, discarded,
unverified (over the verify cap) — and every unit that produced nothing usable is listed
in `failures`.
"""
import json
import os
import re
import shlex
import subprocess
import threading
from collections import defaultdict

META = {"name": "p1-modularity-audit",
        "description": "Audit p1 against its modularity rules with DeepSeek V4.1 Flash workers"}

LINE_BUDGET = 2600
VERIFY_ENV = "deepseek2-audit-max"
REPRO_PROGRAMS = (("rg",), ("grep",), ("git", "grep"), ("cargo", "tree"), ("cargo", "metadata"))
SHELL_META = re.compile(r"[|;&<>`$]")

# seams.md §2: the piece each crate is, and what it must not own.
PIECES = {
    "p1-contracts": "Core contracts — owns request/history items, tool calls/results, provider stream, small control/event types; NOT concrete providers/tools, project settings, terminal types",
    "p1-core": "Agent core — owns one agent's loop, state, cancellation, steering, ordered boundaries; NOT model-specific choices, prompt prose, filesystem implementation, worker scheduler",
    "p1-provider-anthropic": "Provider — owns API/transport/auth translation, streaming, replay metadata, capability validation, cache/reasoning mapping; NOT tool execution, the task prompt or toolset",
    "p1-provider-openai": "Provider (as above)",
    "p1-provider-openai-chat": "Provider (as above)",
    "p1-provider-http": "Shared provider transport helpers (HTTP, SSE, WebSocket, retry) — support for providers; NOT any one provider's wire format",
    "p1-auth": "Credential resolution for providers — NOT provider wire formats, NOT UI",
    "p1-tool-read": "Tool — owns its declaration, argument validation, execution, output semantics; NOT provider wire formats, terminal widgets, sibling tool internals",
    "p1-tool-edit": "Tool (as above)", "p1-tool-write": "Tool (as above)",
    "p1-tool-patch": "Tool (as above)", "p1-tool-search": "Tool (as above)",
    "p1-tool-shell": "Tool (as above)", "p1-tool-finish": "Tool (as above)",
    "p1-tool-delegate": "Optional delegation tool — depends on the worker interface, not its runtime implementation",
    "p1-workers": "Worker service — child lifecycle, handles, completion, cancellation; NOT a mandatory lead role or decomposition policy",
    "p1-workspace": "Shared workspace/filesystem helpers for tools — NOT a mandatory tool bundle",
    "p1-assembly": "Environment assembly — resolves an agent specification to provider, prompt, exact tools, policies; NOT executing turns or implementing those components",
    "p1-model-profile": "Model profile data (efforts, capabilities) shared by assembly and providers",
    "p1-journal": "Session store — append/read committed records; NOT choosing context, routes, goals",
    "p1-context": "Context policy — builds the next model-visible history; NOT storage format, permissions, tool execution",
    "p1-host": "Frontend/host — user input, event rendering, application lifetime, concrete wiring; NOT provider/tool internals",
    "p1-tui": "Terminal UI rendering — NOT provider/tool internals, NOT agent logic",
    "p1-testkit": "Test support — must never be a normal (non-dev) dependency",
    "p1-provider-conformance": "Provider conformance suite — test support, never a normal dependency",
}
SKIP_CRATES = {"p1-live", "p1-tool-tests"}  # test-only crates; covered by the tests index

LENS_BY_CRATE = defaultdict(lambda: "ownership", {
    "p1-core": "core-purity", "p1-contracts": "contract-surface", "p1-host": "composition-root",
    "p1-tui": "ui-boundary", "p1-testkit": "test-support", "p1-provider-conformance": "test-support"})

LENSES = {
    "ownership": "Does the code do only what its piece owns (the row below) and nothing from its 'NOT' list? Does it reach into a sibling's internals? Does a provider execute or define tools, a tool hold provider wire formats or UI types?",
    "core-purity": "Does the agent core name any concrete provider, tool, file format, prompt template, UI or storage, or make a model-specific choice? (AGENTS.md: the core never names a provider, tool, file format, prompt template or UI.)",
    "composition-root": "Is this host code only wiring (parse flags, construct modules, render events, application lifetime)? Report logic that belongs in a module: agent behaviour, provider or wire-format knowledge, tool behaviour, context policy, or anything an embedding caller using p1 as a library would have to copy. Also: a registry, service locator or hidden global.",
    "ui-boundary": "Does the UI crate know about concrete providers, tools or routes, or hold agent or business logic that belongs in the host or core?",
    "contract-surface": "Is p1-contracts minimal and cohesive? Report items that are one module's internals, types that name a concrete provider/tool/format, drift toward a shared-types grab bag; and types duplicated in several crates that should be one contract. Use the usage matrix in the facts.",
    "test-support": "Is this test support kept out of production code paths (only dev-dependencies use it)? Does it expose production behaviour that a production crate should own?",
    "duplication": "Code duplicated across these sibling crates that should be one shared helper (seams §1 allows shared helpers), or a shared helper that couples siblings that should stay independent. Give BOTH locations as evidence.",
    "error-ownership": "Error types crossing crate boundaries: does a provider/wire error type reach the core or a tool, does the core expose errors that name a provider, do From impls couple crates that should be independent?",
    "test-seams": "Can each crate be tested alone? Report dev-dependencies that pull concrete siblings where a contract or testkit would do, tests that reach into another crate's private module, and crates whose tests need the host or assembly.",
}

FINDINGS_SCHEMA = {
    "type": "object", "additionalProperties": False, "required": ["unit", "read", "findings"],
    "properties": {
        "unit": {"type": "string"},
        "read": {"type": "array", "items": {"type": "string"}},
        "findings": {"type": "array", "items": {
            "type": "object", "additionalProperties": False,
            "required": ["id", "claim", "rule", "evidence", "repro", "severity", "fix"],
            "properties": {
                "id": {"type": "string"},
                "claim": {"type": "string"},
                "rule": {"type": "string"},
                "evidence": {"type": "array", "minItems": 1, "items": {
                    "type": "object", "additionalProperties": False,
                    "required": ["file", "line", "quote"],
                    "properties": {"file": {"type": "string"}, "line": {"type": "integer", "minimum": 1},
                                   "quote": {"type": "string"}}}},
                "repro": {"type": "string"},
                "severity": {"type": "string", "enum": ["high", "medium", "low"]},
                "fix": {"type": "string"}}}}}}

VERDICT_SCHEMA = {
    "type": "object", "additionalProperties": False, "required": ["verdict", "reason", "repro_output"],
    "properties": {"verdict": {"type": "string", "enum": ["upheld", "refuted"]},
                   "reason": {"type": "string"}, "repro_output": {"type": "string"}}}

GAPS_SCHEMA = {
    "type": "object", "additionalProperties": False, "required": ["units"],
    "properties": {"units": {"type": "array", "items": {
        "type": "object", "additionalProperties": False,
        "required": ["label", "lens", "files", "question"],
        "properties": {"label": {"type": "string"},
                       "lens": {"type": "string", "enum": sorted(LENSES)},
                       "files": {"type": "array", "minItems": 1, "items": {"type": "string"}},
                       "question": {"type": "string"}}}}}}

SEVERITY = """Severity: high = breaks an owner architecture rule (AGENTS.md "Architecture") or makes a
module impossible to swap; medium = a seams.md rule is bent or a swap needs edits outside the
module and the composition root; low = local smell, easy to fix, no rule broken."""


def git(repo, *args, check=True):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=check).stdout


def line_count(path):
    with open(path, errors="replace") as handle:
        return sum(1 for _ in handle)


# ---- Scout ------------------------------------------------------------------------------

class Scout:
    """Deterministic facts, scripted findings and the validated unit list for one commit."""

    def __init__(self, repo, sha):
        self.repo, self.sha = repo, sha
        meta = json.loads(subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"],
            cwd=repo, capture_output=True, text=True, check=True).stdout)
        self.crates = {}
        for package in meta["packages"]:
            root = os.path.relpath(os.path.dirname(package["manifest_path"]), repo)
            deps = {"normal": [], "dev": []}
            for dep in package["dependencies"]:
                if dep["name"].startswith("p1-"):
                    deps["dev" if dep["kind"] == "dev" else "normal"].append(dep["name"])
            self.crates[package["name"]] = {"root": root, **deps}
        self.files = {name: sorted(self._sources(info["root"])) for name, info in self.crates.items()}

    def _sources(self, root):
        base = os.path.join(self.repo, root, "src")
        for directory, _, names in os.walk(base):
            for name in names:
                if name.endswith(".rs"):
                    yield os.path.relpath(os.path.join(directory, name), self.repo)

    def lines(self, path):
        return line_count(os.path.join(self.repo, path))

    # -- scripted findings: layering computed, not judged --------------------------------

    def metric_findings(self):
        found = []
        allowed = {
            "p1-core": {"p1-contracts"},
            "p1-workers": {"p1-contracts", "p1-core"},
            "p1-tool-delegate": {"p1-contracts", "p1-workers"},
            "p1-tui": {"p1-contracts"},
        }
        for name in self.crates:
            if name.startswith("p1-tool-") and name not in allowed:
                allowed[name] = {"p1-contracts", "p1-workspace"}
            if name.startswith("p1-provider-") and name not in ("p1-provider-http",):
                allowed[name] = {"p1-contracts", "p1-model-profile", "p1-provider-http"}
        concrete = {n for n in self.crates if n.startswith(("p1-tool-", "p1-provider-"))} - {"p1-provider-http"}
        for name, info in sorted(self.crates.items()):
            for dep in info["normal"]:
                reason = None
                if name in allowed and dep not in allowed[name]:
                    reason = f"{name} may depend only on {sorted(allowed[name])} (seams.md §1)"
                elif dep in concrete and name not in ("p1-host", "p1-assembly"):
                    reason = "only the composition root and assembly may depend on a concrete tool or provider (seams.md §1)"
                elif dep in ("p1-testkit", "p1-provider-conformance"):
                    reason = "test support must never be a normal dependency"
                if reason:
                    manifest = os.path.join(info["root"], "Cargo.toml")
                    line, quote = self._manifest_line(manifest, dep)
                    found.append({"id": f"metric-{name}-{dep}", "claim": f"{name} has a normal dependency on {dep}",
                                  "rule": reason, "evidence": [{"file": manifest, "line": line, "quote": quote}],
                                  "repro": f"cargo tree -p {name} -e normal --depth 1", "severity": "high",
                                  "fix": f"remove the dependency on {dep} or record why in an ADR",
                                  "unit": "scout"})
        return found

    def _manifest_line(self, manifest, dep):
        with open(os.path.join(self.repo, manifest)) as handle:
            for number, text in enumerate(handle, 1):
                if text.strip().startswith(dep):
                    return number, text.strip()
        return 1, dep

    # -- facts handed to every worker ------------------------------------------------------

    def facts(self):
        out = [f"# Facts at commit {self.sha} (computed by the workflow, not by a model)", "",
               "## Crate graph — normal dependencies on workspace crates"]
        for name, info in sorted(self.crates.items()):
            out.append(f"- {name} → {', '.join(info['normal']) or '(none)'}")
        out += ["", "## Crate sizes (lines of src/**/*.rs)"]
        for name in sorted(self.crates):
            out.append(f"- {name}: {sum(self.lines(f) for f in self.files[name])}")
        out += ["", "## Swap cost — places that name each tool/provider crate outside itself"]
        for name in sorted(n for n in self.crates if n.startswith(("p1-tool-", "p1-provider-"))):
            ident = name.replace("-", "_")
            hits = git(self.repo, "grep", "-l", "-w", "-e", ident, "-e", name, self.sha, "--",
                       "crates", "Cargo.toml", check=False).split()
            hits = sorted({h.split(":", 1)[1] for h in hits} - set(f for f in self.files[name]))
            hits = [h for h in hits if not h.startswith(self.crates[name]["root"] + "/")]
            code = [h for h in hits if "/src/" in h]
            other = len(hits) - len(code)
            out.append(f"- {name}: {len(code)} source files — {', '.join(code[:12])}"
                       f"{' …' if len(code) > 12 else ''}; plus {other} manifests/tests")
        out += ["", "## p1-contracts usage matrix — item → crates that name it"]
        out += self.usage_matrix()
        return "\n".join(out) + "\n"

    def usage_matrix(self):
        items = set()
        for path in self.files.get("p1-contracts", []):
            with open(os.path.join(self.repo, path)) as handle:
                items |= set(re.findall(r"^pub (?:struct|enum|trait|fn|type) (\w+)", handle.read(), re.M))
        rows = []
        for item in sorted(items):
            users = sorted(name for name, files in self.files.items()
                           if name != "p1-contracts" and any(self._names(f, item) for f in files))
            rows.append(f"- {item}: {len(users)} — {', '.join(users)}")
        return rows

    def _names(self, path, word):
        with open(os.path.join(self.repo, path), errors="replace") as handle:
            return re.search(rf"\b{re.escape(word)}\b", handle.read()) is not None

    # -- units --------------------------------------------------------------------------

    def units(self):
        units = []
        for name in sorted(self.crates):
            if name in SKIP_CRATES:
                continue
            chunks, current, size = [], [], 0
            for path in self.files[name]:
                count = self.lines(path)
                if current and size + count > LINE_BUDGET:
                    chunks.append(current)
                    current, size = [], 0
                current.append(path)
                size += count
            if current:
                chunks.append(current)
            for index, files in enumerate(chunks, 1):
                label = name.removeprefix("p1-") + (f"-{index}" if len(chunks) > 1 else "")
                units.append({"label": label, "lens": LENS_BY_CRATE[name], "files": files,
                              "crate": name})
        src = lambda crate, *names: [f"crates/{crate}/src/{n}" for n in names]
        units += [
            {"label": "dup-provider-request", "lens": "duplication",
             "files": src("p1-provider-anthropic", "request.rs") + src("p1-provider-openai", "request.rs")
             + src("p1-provider-openai-chat", "request.rs")},
            {"label": "dup-provider-parser", "lens": "duplication",
             "files": src("p1-provider-anthropic", "parser.rs") + src("p1-provider-openai", "parser.rs")
             + src("p1-provider-openai-chat", "parser.rs")},
            {"label": "dup-provider-driver", "lens": "duplication",
             "files": src("p1-provider-anthropic", "provider.rs", "lib.rs") + src("p1-provider-openai", "provider.rs", "lib.rs")
             + src("p1-provider-openai-chat", "lib.rs")},
            {"label": "dup-tools-files", "lens": "duplication",
             "files": src("p1-tool-read", "lib.rs") + src("p1-tool-edit", "lib.rs") + src("p1-tool-write", "lib.rs")},
            {"label": "dup-tools-patch-search", "lens": "duplication",
             "files": src("p1-tool-patch", "lib.rs") + src("p1-tool-search", "lib.rs")},
        ]
        return units

    def errors_index(self):
        """Every error enum/struct and every From impl, as file:line entries."""
        out = git(self.repo, "grep", "-n", "-E",
                  r"derive\(.*\bError\b|^\s*impl.* From<.*> for|#\[error\(", self.sha, "--", "crates/*/src/*",
                  check=False)
        entries = []
        for line in out.splitlines():
            _, path, number, text = line.split(":", 3)
            if "#[error(" in text:
                continue
            entries.append(f"{path}:{number}: {text.strip()}")
        return entries

    def tests_index(self):
        out = []
        for name, info in sorted(self.crates.items()):
            if info["dev"]:
                out.append(f"- {name} dev-dependencies: {', '.join(info['dev'])}")
        grep = git(self.repo, "grep", "-n", "-E", r"^use p1_|#\[path|::tests::|pub\(crate\)", self.sha,
                   "--", "crates/*/tests/*", check=False)
        uses = defaultdict(set)
        for line in grep.splitlines():
            _, path, _, text = line.split(":", 3)
            uses[path].add(text.strip())
        for path in sorted(uses):
            out.append(f"- {path}: {'; '.join(sorted(uses[path]))[:300]}")
        return out

    def validate(self, units):
        """The run aborts rather than silently shrinking its coverage."""
        problems = []
        covered = set()
        for unit in units:
            for path in unit["files"]:
                if not os.path.isfile(os.path.join(self.repo, path)):
                    problems.append(f"unit {unit['label']}: no such file {path}")
            size = sum(self.lines(p) for p in unit["files"] if os.path.isfile(os.path.join(self.repo, p)))
            if size > LINE_BUDGET and len(unit["files"]) > 1:
                problems.append(f"unit {unit['label']}: {size} lines exceeds the {LINE_BUDGET}-line budget")
            covered |= set(unit["files"])
        for name, files in self.files.items():
            if name not in SKIP_CRATES and not set(files) <= covered:
                problems.append(f"crate {name}: files in no unit: {sorted(set(files) - covered)}")
        return problems


# ---- checks the workflow runs on worker output --------------------------------------------

def normalise(text):
    return re.sub(r"\s+", " ", text).strip()


def evidence_errors(repo, evidence):
    errors = []
    for item in evidence:
        path = item["file"].removeprefix("./")
        full = os.path.join(repo, path)
        if os.path.isabs(item["file"]) or not os.path.isfile(full):
            errors.append(f"evidence file {item['file']!r} is not a repository-relative path of an existing file")
            continue
        with open(full, errors="replace") as handle:
            lines = handle.read().splitlines()
        window = normalise("\n".join(lines[max(0, item["line"] - 4): item["line"] + 3]))
        quote = normalise(item["quote"])
        if not quote or quote not in window:
            errors.append(f"quote {item['quote'][:80]!r} is not at {path}:{item['line']} (±3 lines)")
    return errors


def repro_output(repo, command):
    """(output, error) — the command runs WITHOUT a shell, in the pinned reference tree."""
    if SHELL_META.search(command):
        return None, f"repro {command!r} must be one command without pipes, redirections or $"
    try:
        argv = shlex.split(command)
    except ValueError as error:
        return None, f"repro {command!r}: {error}"
    if not any(tuple(argv[:len(p)]) == p for p in REPRO_PROGRAMS):
        return None, f"repro {command!r} must start with one of: {', '.join(' '.join(p) for p in REPRO_PROGRAMS)}"
    if argv[0] == "cargo" and "--offline" not in argv:
        argv.append("--offline")
    try:
        done = subprocess.run(argv, cwd=repo, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired) as error:
        return None, f"repro {command!r} failed to run: {error}"
    if not done.stdout.strip():
        return None, f"repro {command!r} printed nothing (exit {done.returncode})"
    return done.stdout[:4000], None


def files_read_in(session_path):
    """Paths the worker actually opened: read-tool paths and any shell command text."""
    seen, shell_text = set(), []
    try:
        handle = open(session_path)
    except OSError:
        return None, ""
    with handle:
        for line in handle:
            record = json.loads(line)
            for block in (record.get("item") or {}).get("blocks", []):
                if block.get("block") != "tool_call":
                    continue
                raw = (block.get("input") or {}).get("raw", "")
                try:
                    args = json.loads(raw)
                except (json.JSONDecodeError, TypeError):
                    args = {}
                if block.get("name") == "read" and isinstance(args, dict):
                    seen.add(str(args.get("file_path", "")))
                elif block.get("name") == "shell":
                    shell_text.append(raw)
    return seen, "\n".join(shell_text)


# ---- the run ------------------------------------------------------------------------------

def run(wf, args):
    repo = os.path.abspath(args.get("repo", os.getcwd()))
    sha = args.get("sha") or git(repo, "rev-parse", "HEAD").strip()
    verify_cap = int(args.get("verify_cap", 30))
    critic_rounds = int(args.get("critic_rounds", 2))
    only = set(filter(None, args.get("only", "").split(",")))  # smoke runs: these unit labels
    trees_root = os.path.abspath(args.get("trees", os.path.join(os.path.dirname(repo), "phaseone-audit-trees")))
    git_lock = threading.Lock()
    state_lock = threading.Lock()

    # The auditor environments are not shipped (they would show up in `p1 models`). p1 finds
    # profiles and routes at `<environments dir>/../{profiles,routes}`, so the run gets its own
    # share directory: the auditor environments plus links to the repository's profiles and routes.
    # (`environments` is a real directory: `..` of a linked directory is the link target's parent.)
    share = os.path.join(wf.out_dir, "p1-share")
    ours = os.path.join(os.path.dirname(os.path.abspath(__file__)), "environments")
    links = {os.path.join(share, "profiles"): os.path.join(repo, "profiles"),
             os.path.join(share, "routes"): os.path.join(repo, "routes")}
    links.update({os.path.join(share, "environments", name): os.path.join(ours, name)
                  for name in os.listdir(ours)})
    os.makedirs(os.path.join(share, "environments"), exist_ok=True)
    for link, target in links.items():
        if os.path.islink(link):
            os.remove(link)
        os.symlink(target, link)
    os.environ["P1_ENVIRONMENTS_DIR"] = os.path.join(share, "environments")
    preflight(wf, repo)

    wf.phase("Scout")
    reference = os.path.join(trees_root, "_reference")
    with git_lock:
        if not os.path.isdir(reference):
            git(repo, "worktree", "add", "--detach", reference, sha)
    scout = Scout(reference, sha)
    units = scout.units()
    problems = scout.validate(units)
    if problems:
        raise wf.Error("scout: " + "; ".join(problems))
    facts = scout.facts()
    errors_index, tests_index = scout.errors_index(), scout.tests_index()
    units += [{"label": "errors", "lens": "error-ownership", "files": [], "index": errors_index},
              {"label": "test-seams", "lens": "test-seams", "files": [], "index": tests_index}]
    with open(os.path.join(wf.out_dir, "facts.md"), "w") as handle:
        handle.write(facts)
    if only:
        units = [u for u in units if u["label"] in only]
        if len(units) != len(only):
            raise wf.Error(f"scout: unknown unit in only={sorted(only)}")
    metrics = [] if only else scout.metric_findings()
    wf.log(f"scout: {len(units)} units, {len(metrics)} scripted findings, commit {sha[:10]}")

    # one detached worktree per job; a dirty tree voids the job
    trees = {}

    def tree_for(label):
        path = os.path.join(trees_root, label)
        with git_lock:
            if not os.path.isdir(path):
                git(repo, "worktree", "add", "--detach", path, sha)
        trees[label] = path
        return path

    def after_run(label):
        path = trees.get(label)
        status = git(path, "status", "--porcelain", "--untracked-files=all")
        if status.strip():
            return f"worker changed its worktree (kept for inspection at {path}): {status.strip()[:300]}"
        with git_lock:
            git(repo, "worktree", "remove", path)
        return None

    seen, confirmed, discarded, unverified = [], [], [], []
    verified_count = [0]

    def is_new(finding):
        head = finding["evidence"][0]
        with state_lock:
            for other in seen:
                first = other["evidence"][0]
                if first["file"] == head["file"] and abs(first["line"] - head["line"]) <= 3:
                    return False
            seen.append(finding)
            return True

    def find_check(unit):
        def check(obj):
            errors = []
            for finding in obj["findings"]:
                errors += [f"{finding['id']}: {e}" for e in evidence_errors(reference, finding["evidence"])]
                _, problem = repro_output(reference, finding["repro"])
                if problem:
                    errors.append(f"{finding['id']}: {problem}")
            listed = {p.removeprefix("./") for p in obj["read"]}
            missing = [f for f in unit["files"] if not any(r.endswith(f) for r in listed)]
            if missing:
                errors.append(f"`read` omits unit files: {missing}")
            return errors
        return check

    def find(unit):
        label = f"find-{unit['label']}"
        brief = find_brief(unit, facts)
        obj = wf.agent(label, brief, FINDINGS_SCHEMA, check=find_check(unit),
                       workspace=tree_for(label), after_run=after_run, phase="Find")
        if obj is None:
            return None
        opened = journal_reads(wf, label)
        if opened is not None:
            unread = [f for f in unit["files"] if not any(o.endswith(f) for o in opened[0]) and f not in opened[1]]
            if unread:
                wf.fail(label, "claimed to read files it never opened", files=unread)
                return None
        for finding in obj["findings"]:
            finding["unit"] = unit["label"]
            finding["lens"] = unit["lens"]
        return [f for f in obj["findings"] if is_new(f)]

    def verify(finding):
        with state_lock:
            if verified_count[0] >= verify_cap:
                unverified.append(finding)
                wf.log(f"verify cap {verify_cap} reached: {finding['id']} left unverified")
                return None
            verified_count[0] += 1
        output, _ = repro_output(reference, finding["repro"])
        votes = wf.parallel([lambda lens=lens: refute(finding, lens, output) for lens in ("rule", "code")])
        upheld = [v for v in votes if v and v["verdict"] == "upheld"]
        if len(upheld) == 1 and finding["severity"] == "high":
            third = refute(finding, "code-second", output)
            votes.append(third)
            upheld += [third] if third and third["verdict"] == "upheld" else []
            keep = len(upheld) >= 2
        else:
            keep = len(upheld) == len(votes) == 2
        record = dict(finding, votes=votes)
        with state_lock:
            (confirmed if keep else discarded).append(record)
        return record

    def refute(finding, lens, output):
        label = SAFE(f"verify-{finding['unit']}-{finding['id']}-{lens}")
        return wf.agent(label, refute_brief(finding, lens, output, facts), VERDICT_SCHEMA,
                        env=VERIFY_ENV, workspace=tree_for(label), after_run=after_run,
                        phase="Verify")

    wf.phase("Find")
    wf.pipeline(metrics, lambda f, *_: verify(f) if is_new(f) else None)
    wf.pipeline(units, lambda unit, *_: find(unit),
                lambda found, *_: wf.pipeline(found, lambda f, *_: verify(f)))

    wf.phase("Critic")
    done_units = list(units)
    for round_number in range(critic_rounds):
        gaps = wf.agent(f"critic-{round_number}", critic_brief(done_units, confirmed, discarded, facts),
                        GAPS_SCHEMA, check=lambda obj: gap_errors(scout, obj),
                        workspace=tree_for(f"critic-{round_number}"), after_run=after_run, phase="Critic")
        if not gaps or not gaps["units"]:
            wf.log(f"critic round {round_number}: nothing new")
            break
        new_units = [dict(u, label=f"gap{round_number}-{SAFE(u['label'])}") for u in gaps["units"][:6]]
        wf.log(f"critic round {round_number}: {len(new_units)} new units")
        done_units += new_units
        wf.pipeline(new_units, lambda unit, *_: find(unit),
                    lambda found, *_: wf.pipeline(found, lambda f, *_: verify(f)))

    with git_lock:
        # trees of jobs reused from an earlier run were never handed to after_run
        for path in trees.values():
            if os.path.isdir(path) and not git(path, "status", "--porcelain", "--untracked-files=all").strip():
                git(repo, "worktree", "remove", path, check=False)
        git(repo, "worktree", "remove", "--force", reference, check=False)
    return {"commit": sha, "units": [{k: u[k] for k in ("label", "lens", "files")} for u in done_units],
            "confirmed": confirmed, "discarded": discarded, "unverified": unverified,
            "facts_file": os.path.join(wf.out_dir, "facts.md")}


def preflight(wf, repo):
    """Resolve every environment the run uses before any job starts: a broken environment
    would otherwise fail every job the same way."""
    binary = os.environ.get("P1_BIN") or os.path.join(os.path.dirname(repo), "phaseone-target", "debug", "p1")
    for env in (wf.env, VERIFY_ENV):
        done = subprocess.run([binary, "env", "show", env], capture_output=True, text=True)
        if done.returncode != 0:
            raise wf.Error(f"preflight: environment {env} does not resolve: {done.stderr.strip()[:500]}")
    wf.log(f"preflight: {wf.env} and {VERIFY_ENV} resolve")


def SAFE(label):
    return re.sub(r"[^A-Za-z0-9._-]+", "-", label)


def journal_reads(wf, label):
    """(paths read with the read tool, shell text) from the job's last session journal."""
    journal = os.path.join(wf.out_dir, "journal.jsonl")
    run_dir = None
    with open(journal) as handle:
        for line in handle:
            record = json.loads(line)
            if record.get("label") == label and record.get("run_dir"):
                run_dir = record["run_dir"]
    if run_dir is None:
        return None  # reused from an earlier run; checked then
    seen, shell = files_read_in(os.path.join(run_dir, "session.jsonl"))
    return None if seen is None else (seen, shell)


def gap_errors(scout, obj):
    errors = []
    for unit in obj["units"][:6]:
        for path in unit["files"]:
            if not os.path.isfile(os.path.join(scout.repo, path)):
                errors.append(f"gap {unit['label']}: no such file {path}")
        size = sum(scout.lines(p) for p in unit["files"] if os.path.isfile(os.path.join(scout.repo, p)))
        if size > LINE_BUDGET:
            errors.append(f"gap {unit['label']}: {size} lines exceeds the {LINE_BUDGET}-line budget; split it")
    if len(obj["units"]) > 6:
        errors.append("at most 6 gap units")
    return errors


# ---- briefs -------------------------------------------------------------------------------

COMMON = """Rules for this job:
- Read the code, not the docs. A document or comment that contradicts the code is a finding
  against the document.
- `findings: []` is an expected, correct answer. Never invent a finding to have one.
- Every finding needs (1) evidence: a quote copied exactly from a repository-relative
  `file` at `line` (the check allows ±3 lines), (2) `rule`: the rule it breaks, named as
  "AGENTS.md Architecture", "seams.md §N" or "ADR-NNNN" — read the rule before citing it,
  and check `docs/adr/` for an accepted ADR that chose this design (then it is NOT a finding),
  (3) `repro`: ONE command starting with `rg`, `grep`, `git grep`, `cargo tree` or
  `cargo metadata`, without pipes, redirections or `$`, whose output shows the problem.
- The workflow re-checks every quote and re-runs every repro; a finding that fails is
  rejected. Accuracy matters more than count.
"""


def find_brief(unit, facts):
    files = unit["files"]
    parts = [f"# Modularity audit — unit `{unit['label']}`, lens `{unit['lens']}`", "",
             "p1 is a modular Rust coding harness. Its modularity rules are AGENTS.md",
             "\"Architecture\" and docs/design/seams.md §1 (dependency rules) and §2 (ownership",
             "table). Read both first.", "", "## Question", LENSES[unit["lens"]]]
    if unit.get("question"):
        parts += ["", f"Critic's question for this unit: {unit['question']}"]
    if unit.get("crate") in PIECES:
        parts += ["", f"This crate's piece: {PIECES[unit['crate']]}"]
    if files:
        parts += ["", "## Files — read EVERY one in full with the read tool (the session is checked)"]
        parts += [f"- {path}" for path in files]
        parts += ["", "You may open any other file to follow a caller or a type."]
    if unit.get("index"):
        parts += ["", "## Index (every entry is a lead to open; list each file you open in `read`)"]
        parts += unit["index"][:400]
    parts += ["", SEVERITY, "", COMMON, "", "## Facts", facts]
    return "\n".join(parts)


REFUTER = {
    "rule": "RULE lens: does a rule really forbid this? Read the cited rule and search docs/adr/ and DECISIONS.md for a decision that chose this design; seams.md §1 allows shared helpers.",
    "code": "CODE lens: open the evidence and its callers. Is the claim true of the code as it is now, and is the proposed fix possible without breaking a consumer?",
    "code-second": "CODE lens (independent second look): open the evidence and its callers yourself. Is the claim true, and does it matter for swapping or testing a module?",
}


def refute_brief(finding, lens, output, facts):
    shown = {k: finding[k] for k in ("id", "claim", "rule", "evidence", "repro", "severity", "fix")}
    return "\n".join([
        "# Modularity audit — try to REFUTE one finding", "",
        "Another auditor claims the finding below. Your job is to refute it. Answer `refuted`",
        "if it is false, not a rule violation, already decided in an ADR, or if you are unsure.",
        "Answer `upheld` only if you checked the code and the rule yourself and it holds.", "",
        REFUTER[lens], "",
        "Re-run the repro yourself and put its output (shortened) in `repro_output`.", "",
        "## Finding", "```json", json.dumps(shown, indent=2), "```", "",
        "## The repro's output when the workflow ran it", "```", (output or "(no output)")[:3000], "```", "",
        "## Facts", facts])


def critic_brief(units, confirmed, discarded, facts):
    coverage = "\n".join(f"- {u['label']} ({u['lens']}): {', '.join(u['files']) or 'index'}" for u in units)
    found = "\n".join(f"- [{f['severity']}] {f['claim']}" for f in confirmed) or "(none)"
    rejected = "\n".join(f"- {f['claim']}" for f in discarded) or "(none)"
    return "\n".join([
        "# Modularity audit — completeness critic", "",
        "Below is what the audit covered. Find what it MISSED: a modularity rule (AGENTS.md",
        "Architecture, docs/design/seams.md) that no unit tested, a crate boundary nobody looked",
        "across, a pair of crates that should have been compared. Propose at most 6 new units,",
        "each a list of repository-relative files within 2600 lines together, one lens and a",
        "specific question. `units: []` is the right answer if coverage is complete.", "",
        "## Coverage (unit, lens, files)", coverage, "",
        "## Confirmed findings", found, "", "## Discarded findings", rejected, "",
        "## Facts", facts])
