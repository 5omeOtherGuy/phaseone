# Dogfooding evidence

One JSON record per supervised run in `runs.jsonl`, written by
`scripts/run-report.py SESSION.jsonl --append docs/dogfood/runs.jsonl …`. The journal supplies
the counts; the operator supplies what only they know: `--accepted` (after INDEPENDENT
verification — re-run the checks yourself), `--interventions`, `--elapsed`, `--exit-code`.

Rules (review 2026-09-20, plan amendments 3 and 6):
- Real tasks run in a disposable clone or worktree, never in a checkout someone works in.
- A non-zero shell exit is not a failed tool call; both are recorded, separately.
- Worker tokens are not in the parent's journal (`includes_worker_usage: false`) until child
  usage is surfaced; say so when quoting totals.
- Failures are grouped into GitHub issues; a failure that can be reproduced becomes a test.
