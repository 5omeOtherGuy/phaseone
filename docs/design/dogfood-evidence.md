# Dogfood inspection evidence

`scripts/dogfood.sh` executes against a disposable clone. It retains the raw session
journal, stdout and stderr in the run directory (`../phaseone-dogfood/<label>.run/`) so
the independent review required by the project `AGENTS.md` can inspect them. That
directory is created owner-only (mode 0700) and the evidence files are mode 0600; the
task prompt file itself is never copied into the run directory. The disposable
invocation scratch still holds only the diff numstat and is removed at exit.

`report.json` and `review-evidence.json` sit alongside the raw evidence.
`review-evidence.json` records process exit status, ordered tool outcomes and shell
exit codes, stdout/stderr byte and error-line counts, and aggregate changed-file/
insertion/deletion counts, so a machine check can read the outcome without parsing the
raw streams. When a failure's cause needs textual inspection, the reviewer reads the
retained journal, stdout and stderr in the same run directory.

The clone itself contains the agent's changes and remains agent-controlled and private;
it is not part of the retained evidence and is removed with the disposable clone.

The separate `scripts/fanout.py` p1 runner keeps the original brief in `task.txt`,
per its frozen `test_run_dir_layout` contract. Its run directory is mode 0700 and
its task, journal, stdout, stderr and report files are mode 0600, including files
created on resume. The child receives an owner-only umask.
