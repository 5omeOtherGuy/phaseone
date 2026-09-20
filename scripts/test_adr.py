#!/usr/bin/env python3
"""Unit tests for scripts/adr.py — stdlib unittest, temp dirs only.

    python3 scripts/test_adr.py [-q]

Every test passes --dir (or P1_ADR_DIR) at a temporary ADR directory, so the real
docs/adr is never written to. Each check failure in adr.py has a minimal broken
fixture below.
"""
from __future__ import annotations

import contextlib
import io
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import adr  # noqa: E402

TEMPLATE = """---
adr: {{number}}
title: {{title}}
status: proposed
date: {{date}}
deciders: {{deciders}}
supersedes: [{{supersedes}}]
superseded_by: []
sources: []
---
# ADR-{{number_padded}}: {{title}}

## Context

<context placeholder>

## Decision

<decision placeholder>

## Consequences

<consequences placeholder>

## Alternatives considered

<alternatives placeholder>

## Evidence

<evidence placeholder>
"""

README = """# Architecture Decision Records

## Index

<!-- adr-index:start -->
<!-- adr-index:end -->
"""


def adr_text(number: int, title: str, *, status: str = "accepted",
             date: str = "2026-09-20", deciders: str = "lead",
             supersedes: str = "[]", superseded_by: str = "[]",
             sources: str = "[D1]",
             h1: str | None = None, body: str | None = None) -> str:
    if body is None:
        body = ("\n\n## Context\n\nContext text.\n"
                "\n## Decision\n\nDecision text.\n"
                "\n## Consequences\n\nConsequences text.\n"
                "\n## Alternatives considered\n\nNone recorded.\n"
                "\n## Evidence\n\nNone recorded.\n")
    heading = h1 if h1 is not None else f"# ADR-{number:04d}: {title}"
    return (f"---\nadr: {number}\ntitle: {title}\nstatus: {status}\ndate: {date}\n"
            f"deciders: {deciders}\nsupersedes: {supersedes}\n"
            f"superseded_by: {superseded_by}\nsources: {sources}\n---\n"
            f"{heading}{body}")


class AdrTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="adr-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        with open(os.path.join(self.dir, "template.md"), "w", encoding="utf-8") as handle:
            handle.write(TEMPLATE)
        with open(os.path.join(self.dir, "README.md"), "w", encoding="utf-8") as handle:
            handle.write(README)

    def write(self, filename: str, text: str) -> str:
        path = os.path.join(self.dir, filename)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def read(self, path: str) -> str:
        with open(path, encoding="utf-8") as handle:
            return handle.read()

    def write_valid(self, number: int, *, title: str | None = None, **kwargs) -> str:
        title = title or f"Decision {number}"
        return self.write(f"{number:04d}-decision-{number}.md",
                          adr_text(number, title, **kwargs))

    def reindex(self) -> None:
        self.assertEqual(adr.main(["index", "--dir", self.dir]), 0)

    def new(self, *args: str) -> str:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(adr.main(["new", "--dir", self.dir, *args]), 0)
        path = out.getvalue().strip()
        self.assertTrue(os.path.isfile(path), path)
        return path

    def fill_placeholders(self, path: str) -> None:
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        text = "\n".join(
            "Real content." if adr.PLACEHOLDER_RE.match(line) else line
            for line in text.splitlines()) + "\n"
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)

    def problems(self) -> list[str]:
        return adr.check_dir(self.dir)

    def assertProblem(self, needle: str) -> None:
        problems = self.problems()
        self.assertTrue(any(needle in p for p in problems),
                        f"expected a problem containing {needle!r}, got {problems}")

    # --- new ---------------------------------------------------------------

    def test_new_numbering_is_dense_from_one(self) -> None:
        first = self.new("First decision")
        self.assertTrue(first.endswith("0001-first-decision.md"))
        second = self.new("Second decision")
        self.assertTrue(second.endswith("0002-second-decision.md"))
        third = self.new("Third decision", "--deciders", "owner")
        self.assertTrue(third.endswith("0003-third-decision.md"))
        with open(third, encoding="utf-8") as handle:
            self.assertIn("deciders: owner", handle.read())

    def test_new_supersedes_sets_reciprocity(self) -> None:
        self.write_valid(1)
        path = self.new("Newer", "--supersedes", "1")
        self.fill_placeholders(path)
        self.reindex()
        self.assertEqual(self.problems(), [])
        old = self.read(os.path.join(self.dir, "0001-decision-1.md"))
        self.assertIn("status: superseded", old)
        self.assertIn("superseded_by: [2]", old)
        new = self.read(path)
        self.assertIn("supersedes: [1]", new)

    # --- index -------------------------------------------------------------

    def test_index_is_idempotent(self) -> None:
        self.write_valid(1)
        self.write_valid(2)
        self.reindex()
        readme = os.path.join(self.dir, "README.md")
        first = self.read(readme)
        self.reindex()
        self.assertEqual(first, self.read(readme))

    def test_index_marks_superseded(self) -> None:
        self.write_valid(1, status="superseded", superseded_by="[2]")
        self.write_valid(2, supersedes="[1]")
        self.reindex()
        readme = self.read(os.path.join(self.dir, "README.md"))
        self.assertIn("superseded by ADR-0002", readme)

    # --- check: numbering and filenames ------------------------------------

    def test_check_numbering_gap(self) -> None:
        self.write_valid(1)
        self.write_valid(3)
        self.reindex()
        self.assertProblem("not dense: missing ADR-0002")

    def test_check_number_filename_mismatch(self) -> None:
        self.write("0001-x.md", adr_text(2, "X"))
        self.assertProblem("does not match adr: 2")

    # --- check: front matter ----------------------------------------------

    def test_check_missing_key(self) -> None:
        text = adr_text(1, "X").replace("sources: [D1]\n", "")
        self.write("0001-x.md", text)
        self.assertProblem("missing front-matter key(s): sources")

    def test_check_unknown_key(self) -> None:
        text = adr_text(1, "X").replace("sources: [D1]", "sources: [D1]\nextra: 1")
        self.write("0001-x.md", text)
        self.assertProblem("unknown front-matter key(s): extra")

    def test_check_bad_status(self) -> None:
        self.write("0001-x.md", adr_text(1, "X", status="maybe"))
        self.assertProblem("bad status: 'maybe'")

    def test_check_bad_date(self) -> None:
        self.write("0001-x.md", adr_text(1, "X", date="20-09-2026"))
        self.assertProblem("bad date")

    def test_check_bad_deciders(self) -> None:
        self.write("0001-x.md", adr_text(1, "X", deciders="team"))
        self.assertProblem("bad deciders: 'team'")

    def test_check_h1_mismatch(self) -> None:
        self.write("0001-x.md", adr_text(1, "X", h1="# ADR-0002: X"))
        self.assertProblem("does not match expected")

    # --- check: sections ---------------------------------------------------

    def test_check_missing_section(self) -> None:
        body = "\n\n## Context\n\nC.\n\n## Decision\n\nD.\n\n## Consequences\n\nC.\n"
        self.write("0001-x.md", adr_text(1, "X", body=body))
        self.assertProblem("missing section ## Alternatives considered")

    def test_check_empty_section(self) -> None:
        body = ("\n\n## Context\n\nC.\n\n## Decision\n\nD.\n\n## Consequences\n\nC.\n"
                "\n## Alternatives considered\n\n\n## Evidence\n\nE.\n")
        self.write("0001-x.md", adr_text(1, "X", body=body))
        self.assertProblem("empty section ## Alternatives considered")

    def test_check_out_of_order_section(self) -> None:
        body = ("\n\n## Context\n\nC.\n\n## Evidence\n\nE.\n\n## Decision\n\nD.\n"
                "\n## Consequences\n\nC.\n\n## Alternatives considered\n\nN.\n")
        self.write("0001-x.md", adr_text(1, "X", body=body))
        self.assertProblem("out of order")

    def test_check_placeholder_text(self) -> None:
        body = ("\n\n## Context\n\n<still a placeholder>\n\n## Decision\n\nD.\n"
                "\n## Consequences\n\nC.\n\n## Alternatives considered\n\nN.\n"
                "\n## Evidence\n\nE.\n")
        self.write("0001-x.md", adr_text(1, "X", body=body))
        self.assertProblem("placeholder")

    # --- check: supersede chain -------------------------------------------

    def test_check_supersedes_missing_adr(self) -> None:
        self.write_valid(1, supersedes="[9]")
        self.reindex()
        self.assertProblem("supersedes ADR-0009, which does not exist")

    def test_check_supersedes_not_reciprocal(self) -> None:
        self.write_valid(1)
        self.write_valid(2, supersedes="[1]")
        self.reindex()
        self.assertProblem("does not list ADR-0002 in superseded_by")

    def test_check_superseded_without_superseded_by(self) -> None:
        self.write_valid(1, status="superseded")
        self.reindex()
        self.assertProblem("status superseded but superseded_by is empty")

    def test_check_superseded_by_without_status(self) -> None:
        self.write_valid(1, superseded_by="[2]")
        self.write_valid(2, supersedes="[1]")
        self.reindex()
        self.assertProblem("superseded_by is set but status is 'accepted'")

    # --- check: index and links -------------------------------------------

    def test_check_stale_index(self) -> None:
        self.write_valid(1)
        self.write_valid(2)
        self.reindex()
        self.write_valid(2, title="A different title")
        self.assertProblem("index is stale")

    def test_check_broken_relative_link(self) -> None:
        body = ("\n\n## Context\n\nSee [the spec](../design/nope.md).\n\n## Decision\n\nD.\n"
                "\n## Consequences\n\nC.\n\n## Alternatives considered\n\nN.\n"
                "\n## Evidence\n\nNone recorded.\n")
        self.write("0001-x.md", adr_text(1, "X", body=body))
        self.reindex()
        self.assertProblem("relative link to missing file: ../design/nope.md")

    # --- misc --------------------------------------------------------------

    def test_env_dir_is_used(self) -> None:
        self.write_valid(1)
        self.reindex()
        os.environ["P1_ADR_DIR"] = self.dir
        self.addCleanup(os.environ.pop, "P1_ADR_DIR", None)
        self.assertEqual(adr.main(["check"]), 0)

    def test_list_filters_by_status(self) -> None:
        self.write_valid(1)
        self.write_valid(2, status="rejected")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(adr.main(["list", "--status", "rejected", "--dir", self.dir]), 0)
        self.assertIn("ADR-0002", out.getvalue())
        self.assertNotIn("ADR-0001", out.getvalue())


if __name__ == "__main__":
    unittest.main()
