# Dogfooding evidence

One JSON record per supervised run in `runs.jsonl`, written by
`scripts/run-report.py SESSION.jsonl --append docs/dogfood/runs.jsonl …`. The journal supplies
the counts; the operator supplies what only they know: `--accepted` (after INDEPENDENT
verification — re-run the checks yourself), `--interventions`, `--elapsed`, `--exit-code`.

Rules (review 2026-09-20, plan amendments 3 and 6):
- Real tasks run in a disposable clone or worktree, never in a checkout someone works in.
- A non-zero shell exit is not a failed tool call; both are recorded, separately.
- Worker usage is durable: with `--session FILE` each worker `w<N>` journals to its own
  `FILE.w<N>.jsonl`, and `run-report.py` discovers those files and adds their tokens up in
  `usage_with_workers` / `input_total_with_workers` (`includes_worker_usage: true`). A worker
  started without `--session` stays in memory: quote its tokens from the host's
  `workers total …` line, not from the record.
- Failures are grouped into GitHub issues; a failure that can be reproduced becomes a test.
