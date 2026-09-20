#!/usr/bin/env python3
"""Architecture Decision Records for p1 — stdlib only.

Layout: docs/adr/NNNN-kebab-case-title.md plus template.md and README.md.
Front matter is a small fixed subset of YAML, parsed by hand; the body has the
five required sections in a fixed order.

    scripts/adr.py new "Some decision"            # next number, status proposed
    scripts/adr.py new "Reverse it" --supersedes 7
    scripts/adr.py index                          # regenerate README's index block
    scripts/adr.py check                          # exit 1, one line per problem
    scripts/adr.py list [--status accepted]

--dir (or P1_ADR_DIR) points at another ADR directory; the unit tests use it so
the real docs/adr is never written to by tests.
"""
from __future__ import annotations

import argparse
import datetime
import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_DIR = os.path.join(REPO_ROOT, "docs", "adr")

STATUSES = ("proposed", "accepted", "superseded", "rejected")
DECIDERS = ("owner", "lead", "owner+lead")
REQUIRED_KEYS = ("adr", "title", "status", "date", "deciders",
                 "supersedes", "superseded_by", "sources")
SECTIONS = ("Context", "Decision", "Consequences",
            "Alternatives considered", "Evidence")
INDEX_START = "<!-- adr-index:start -->"
INDEX_END = "<!-- adr-index:end -->"

ADR_FILE_RE = re.compile(r"^(\d{4})-(.+)\.md$")
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
H1_RE = re.compile(r"^# (.+)$")
H2_RE = re.compile(r"^## (.+)$")
LINK_RE = re.compile(r"\[[^\]]*\]\(([^)]+)\)")
# A whole line that is still a placeholder from template.md.
PLACEHOLDER_RE = re.compile(r"^\s*(<[^>]*>|\{\{[^}]*\}\}|TODO\b.*|TBD\b.*)\s*$")


class Adr:
    """One ADR file: front matter parsed, body kept as text."""

    def __init__(self, path: str, filename: str) -> None:
        self.path = path
        self.filename = filename
        self.meta: dict[str, str] = {}
        self.title = ""
        self.status = ""
        self.date = ""
        self.deciders = ""
        self.supersedes: list[int] = []
        self.superseded_by: list[int] = []
        self.sources: list[str] = []
        self.body = ""
        self.adr = 0


def parse_list(value: str) -> list[str]:
    value = value.strip()
    if not (value.startswith("[") and value.endswith("]")):
        return [value] if value else []
    inner = value[1:-1].strip()
    if not inner:
        return []
    return [item.strip() for item in inner.split(",") if item.strip()]


def parse_int_list(value: str) -> tuple[list[int] | None, str | None]:
    out: list[int] = []
    for item in parse_list(value):
        try:
            out.append(int(item))
        except ValueError:
            return None, item
    return out, None


def load(path: str, filename: str) -> Adr:
    adr = Adr(path, filename)
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise ValueError("missing front matter")
    end = None
    for i in range(1, len(lines)):
        if lines[i].strip() == "---":
            end = i
            break
    if end is None:
        raise ValueError("front matter not closed")
    for line in lines[1:end]:
        if not line.strip() or line.strip().startswith("#"):
            continue
        if ":" not in line:
            raise ValueError(f"bad front-matter line: {line}")
        key, value = line.split(":", 1)
        adr.meta[key.strip()] = value.strip()
    adr.body = "\n".join(lines[end + 1:])

    missing = [k for k in REQUIRED_KEYS if k not in adr.meta]
    unknown = [k for k in adr.meta if k not in REQUIRED_KEYS]
    problems: list[str] = []
    if missing:
        problems.append("missing front-matter key(s): " + ", ".join(missing))
    if unknown:
        problems.append("unknown front-matter key(s): " + ", ".join(unknown))

    number_prefix = int(filename[:4])
    try:
        adr.adr = int(adr.meta.get("adr", ""))
    except ValueError:
        problems.append(f"bad adr: {adr.meta.get('adr', '')!r} (expected a number)")
        adr.adr = number_prefix
    if adr.adr != number_prefix:
        problems.append(f"filename {filename} does not match adr: {adr.adr}")
    adr.title = adr.meta.get("title", "")
    adr.status = adr.meta.get("status", "")
    adr.date = adr.meta.get("date", "")
    adr.deciders = adr.meta.get("deciders", "")

    if not adr.title:
        problems.append("empty title")
    if adr.status not in STATUSES:
        problems.append(f"bad status: {adr.status!r} (expected one of {', '.join(STATUSES)})")
    if not DATE_RE.match(adr.date):
        problems.append(f"bad date: {adr.date!r} (expected YYYY-MM-DD)")
    else:
        try:
            if datetime.date.fromisoformat(adr.date).isoformat() != adr.date:
                raise ValueError
        except ValueError:
            problems.append(f"bad date: {adr.date!r} (expected YYYY-MM-DD)")
    if adr.deciders not in DECIDERS:
        problems.append(f"bad deciders: {adr.deciders!r} (expected one of {', '.join(DECIDERS)})")

    for key in ("supersedes", "superseded_by"):
        value = adr.meta.get(key)
        if value is None:
            continue
        parsed, bad = parse_int_list(value)
        if parsed is None:
            problems.append(f"bad {key}: {bad!r} is not an ADR number")
        else:
            setattr(adr, key, parsed)
    adr.sources = parse_list(adr.meta.get("sources", ""))

    adr.problems = problems
    return adr


def load_all(directory: str) -> list[Adr]:
    adrs: list[Adr] = []
    if not os.path.isdir(directory):
        return adrs
    for filename in sorted(os.listdir(directory)):
        if ADR_FILE_RE.match(filename):
            try:
                adrs.append(load(os.path.join(directory, filename), filename))
            except ValueError as exc:
                broken = Adr(os.path.join(directory, filename), filename)
                broken.adr = int(filename[:4])
                broken.problems = [str(exc)]
                adrs.append(broken)
    return adrs


def section_problems(adr: Adr) -> list[str]:
    """Missing, empty, placeholder, out-of-order or unknown ## sections."""
    problems: list[str] = []
    lines = adr.body.splitlines()
    h1 = next((line for line in lines if H1_RE.match(line)), None)
    if h1 is None:
        problems.append("missing H1")
    else:
        expected = f"# ADR-{adr.adr:04d}: {adr.title}"
        if h1.strip() != expected:
            problems.append(f"H1 {h1.strip()!r} does not match expected {expected!r}")

    headings: list[tuple[str, int, list[str]]] = []
    seen: dict[str, int] = {}
    for i, line in enumerate(lines):
        match = H2_RE.match(line)
        if not match:
            continue
        name = match.group(1).strip()
        headings.append((name, i, []))
        seen[name] = seen.get(name, 0) + 1
    for idx, (name, start, _) in enumerate(headings):
        stop = headings[idx + 1][1] if idx + 1 < len(headings) else len(lines)
        content = lines[start + 1:stop]
        for line in content:
            if line.strip():
                headings[idx][2].append(line)

    known = [name for name, _, _ in headings]
    for name in SECTIONS:
        if name not in known:
            problems.append(f"missing section ## {name}")
            continue
        _, _, content = headings[known.index(name)]
        if not content:
            problems.append(f"empty section ## {name}")
        elif any(PLACEHOLDER_RE.match(line) for line in content):
            problems.append(f"section ## {name} still contains template placeholder text")
    for name in known:
        if name not in SECTIONS:
            problems.append(f"unknown section ## {name}")
        if known.count(name) > 1:
            problems.append(f"duplicate section ## {name}")
            break
    rank = -1
    for name in known:
        if name not in SECTIONS:
            continue
        here = SECTIONS.index(name)
        if here < rank:
            problems.append(f"section ## {name} is out of order")
        rank = max(rank, here)
    if known != [name for name in SECTIONS if name in known]:
        problems.append("sections are not in the required order")
    return problems


def link_problems(adr: Adr) -> list[str]:
    problems: list[str] = []
    for match in LINK_RE.finditer(adr.body):
        target = match.group(1).strip()
        if target.startswith("<") and target.endswith(">"):
            target = target[1:-1]
        if " " in target:
            target = target.split()[0]
        if target.startswith("#") or "://" in target or target.startswith("mailto:"):
            continue
        target = target.split("#", 1)[0]
        if not target:
            continue
        resolved = os.path.normpath(os.path.join(os.path.dirname(adr.path), target))
        if not os.path.exists(resolved):
            problems.append(f"relative link to missing file: {target}")
    return problems


def generate_index(adrs: list[Adr]) -> str:
    rows = ["| ADR | Title | Status | Date | Deciders |",
            "|---|---|---|---|---|"]
    for adr in sorted(adrs, key=lambda a: a.adr):
        status = adr.status
        if adr.superseded_by:
            status = "superseded by " + ", ".join(
                f"ADR-{number:04d}" for number in adr.superseded_by)
        rows.append(f"| ADR-{adr.adr:04d} | [{adr.title}]({adr.filename}) | "
                    f"{status} | {adr.date} | {adr.deciders} |")
    return "\n".join(rows)


def check_dir(directory: str) -> list[str]:
    problems: list[str] = []
    adrs = load_all(directory)
    by_number: dict[int, Adr] = {}

    for adr in adrs:
        for problem in adr.problems + section_problems(adr) + link_problems(adr):
            problems.append(f"{adr.filename}: {problem}")
        if adr.adr in by_number:
            problems.append(f"{adr.filename}: duplicate ADR number {adr.adr:04d}")
        else:
            by_number[adr.adr] = adr

    numbers = sorted(by_number)
    if numbers:
        for expected in range(1, max(numbers) + 1):
            if expected not in by_number:
                problems.append(f"ADR system: numbering is not dense: missing ADR-{expected:04d}")

    for adr in adrs:
        if adr.adr not in by_number:
            continue
        if adr.status == "superseded" and not adr.superseded_by:
            problems.append(f"{adr.filename}: status superseded but superseded_by is empty")
        if adr.status != "superseded" and adr.superseded_by:
            problems.append(f"{adr.filename}: superseded_by is set but status is {adr.status!r}")
        for number in adr.supersedes:
            other = by_number.get(number)
            if other is None:
                problems.append(f"{adr.filename}: supersedes ADR-{number:04d}, which does not exist")
            elif adr.adr not in other.superseded_by:
                problems.append(f"{adr.filename}: supersedes ADR-{number:04d}, but that ADR "
                                f"does not list ADR-{adr.adr:04d} in superseded_by")
            if number == adr.adr:
                problems.append(f"{adr.filename}: lists itself in supersedes")
        for number in adr.superseded_by:
            other = by_number.get(number)
            if other is None:
                problems.append(f"{adr.filename}: superseded_by ADR-{number:04d}, which does not exist")
            elif adr.adr not in other.supersedes:
                problems.append(f"{adr.filename}: superseded_by ADR-{number:04d}, but that ADR "
                                f"does not list ADR-{adr.adr:04d} in supersedes")

    readme = os.path.join(directory, "README.md")
    if not os.path.isfile(readme):
        problems.append("README.md: missing (run scripts/adr.py index)")
    else:
        with open(readme, encoding="utf-8") as handle:
            text = handle.read()
        if INDEX_START not in text or INDEX_END not in text:
            problems.append("README.md: index markers are missing")
        else:
            current = text.split(INDEX_START, 1)[1].split(INDEX_END, 1)[0].strip()
            expected = generate_index(adrs)
            if current != expected:
                problems.append("README.md: index is stale: run scripts/adr.py index")
    return problems


def readme_with_index(directory: str, adrs: list[Adr]) -> str:
    readme = os.path.join(directory, "README.md")
    with open(readme, encoding="utf-8") as handle:
        text = handle.read()
    if INDEX_START not in text or INDEX_END not in text:
        raise SystemExit(f"adr: {readme} has no {INDEX_START} / {INDEX_END} markers")
    before, rest = text.split(INDEX_START, 1)
    _, after = rest.split(INDEX_END, 1)
    return f"{before}{INDEX_START}\n{generate_index(adrs)}\n{INDEX_END}{after}"


def write_index(directory: str) -> None:
    adrs = load_all(directory)
    readme = os.path.join(directory, "README.md")
    text = readme_with_index(directory, adrs)
    with open(readme, "w", encoding="utf-8") as handle:
        handle.write(text)


def set_meta(path: str, updates: dict[str, str]) -> None:
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    end = next(i for i in range(1, len(lines)) if lines[i].strip() == "---")
    for i in range(1, end):
        if ":" not in lines[i]:
            continue
        key = lines[i].split(":", 1)[0].strip()
        if key in updates:
            lines[i] = f"{key}: {updates[key]}"
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")


def template_text(directory: str, number: int, title: str, deciders: str,
                  supersedes: list[int], date: str) -> str:
    path = os.path.join(directory, "template.md")
    if not os.path.isfile(path):
        raise SystemExit(f"adr: template not found: {path}")
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    replacements = {
        "{{number_padded}}": f"{number:04d}",
        "{{number}}": str(number),
        "{{title}}": title,
        "{{date}}": date,
        "{{deciders}}": deciders,
        "{{supersedes}}": ", ".join(str(n) for n in supersedes),
    }
    for key, value in replacements.items():
        text = text.replace(key, value)
    return text


def slug(title: str) -> str:
    slugged = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
    return slugged or "adr"


def cmd_new(args: argparse.Namespace, directory: str) -> int:
    adrs = load_all(directory)
    by_number = {a.adr: a for a in adrs}
    number = max(by_number, default=0) + 1
    for old_number in args.supersedes:
        if old_number not in by_number:
            raise SystemExit(f"adr: cannot supersede ADR-{old_number:04d}: it does not exist")
    date = args.date or datetime.date.today().isoformat()
    text = template_text(directory, number, args.title, args.deciders, args.supersedes, date)
    path = os.path.join(directory, f"{number:04d}-{slug(args.title)}.md")
    if os.path.exists(path):
        raise SystemExit(f"adr: {path} already exists")
    os.makedirs(directory, exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    for old_number in args.supersedes:
        old = by_number[old_number]
        updated = list(old.superseded_by)
        if number not in updated:
            updated.append(number)
        set_meta(old.path, {
            "status": "superseded",
            "superseded_by": "[" + ", ".join(str(n) for n in updated) + "]",
        })
    print(path)
    return 0


def cmd_index(args: argparse.Namespace, directory: str) -> int:
    write_index(directory)
    return 0


def cmd_check(args: argparse.Namespace, directory: str) -> int:
    problems = check_dir(directory)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        return 1
    return 0


def cmd_list(args: argparse.Namespace, directory: str) -> int:
    for adr in sorted(load_all(directory), key=lambda a: a.adr):
        if args.status and adr.status != args.status:
            continue
        print(f"ADR-{adr.adr:04d}  {adr.status:10s}  {adr.date}  {adr.title}")
    return 0


def resolve_dir(args: argparse.Namespace) -> str:
    return os.path.abspath(getattr(args, "dir", None) or os.environ.get("P1_ADR_DIR") or DEFAULT_DIR)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="adr.py", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    def with_dir(sp: argparse.ArgumentParser) -> argparse.ArgumentParser:
        sp.add_argument("--dir", help="ADR directory (default: docs/adr or $P1_ADR_DIR)")
        return sp

    new = with_dir(sub.add_parser("new", help="create the next ADR from the template"))
    new.add_argument("title")
    new.add_argument("--deciders", default="lead", choices=DECIDERS)
    new.add_argument("--supersedes", type=int, nargs="*", default=[], metavar="N")
    new.add_argument("--date", help="override today's date (YYYY-MM-DD)")

    with_dir(sub.add_parser("index", help="regenerate the index block in README.md"))
    with_dir(sub.add_parser("check", help="validate every ADR and the index"))

    listing = with_dir(sub.add_parser("list", help="list ADRs"))
    listing.add_argument("--status", choices=STATUSES)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    directory = resolve_dir(args)
    if args.command == "new":
        return cmd_new(args, directory)
    if args.command == "index":
        return cmd_index(args, directory)
    if args.command == "check":
        return cmd_check(args, directory)
    return cmd_list(args, directory)


if __name__ == "__main__":
    sys.exit(main())
