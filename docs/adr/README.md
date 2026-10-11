# Architecture Decision Records

One file per settled decision: `NNNN-kebab-case-title.md`, numbered from `0001`; a
number is unique and reserved on the board before the file is written, and pull requests
merge in any order, so a number not merged yet leaves a gap (ADR-0107). The front matter records who decided and where it came from; the body records
the context, the decision, its consequences, the alternatives and the evidence.
`DECISIONS.md` is the frozen ledger of the first slice; these ADRs are the record
from here on.

Run `scripts/adr.py check` (wired into `scripts/gate.sh`) to validate. Read every
ADR through `scripts/adr.py list` or `scripts/adr.py index`.

## Index

The index is generated, not committed (ADR-0191): a committed table made every two pull
requests with a record conflict here. `scripts/adr.py list` prints number, status, date and
title; `scripts/adr.py index` prints the full Markdown table with links and deciders. The file
names above are the titles.

## Writing one

1. `scripts/adr.py new "Short decision title" [--deciders owner|lead|owner+lead] [--supersedes N …]`.
   The file is created with status `proposed`, today's date and the template's sections.
2. Fill every section; delete the `<…>` placeholders. An owner decision says so in
   Context and quotes the owner's words when a source has them. If no alternative or
   evidence is recorded, write `None recorded.` — never invent one.
3. Keep the status `proposed` until the change is merged; then set it to `accepted`
   (or `rejected`).
4. Never edit an accepted ADR except its `status` and `superseded_by`. To reverse a
   decision, write a NEW ADR with `--supersedes N`; the old one becomes `superseded`.
5. `scripts/adr.py check`. No index to update.
