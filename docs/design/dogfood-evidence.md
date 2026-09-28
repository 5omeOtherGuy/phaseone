# Dogfood inspection evidence

`scripts/dogfood.sh` executes against a disposable clone. Its private invocation scratch
holds the task prompt, session journal, stdout, stderr and diff numstat only until the
invocation has generated `report.json` and `review-evidence.json`; cleanup removes the
raw scratch. No raw prompt, model message, tool argument, stderr line, diff text or
changed filename is exported into the retained `.run/` directory.

An independent reviewer reads `report.json` and `review-evidence.json` before deciding
acceptance: the latter records process exit status, ordered tool outcomes and shell exit
codes, stdout/stderr byte and error-line counts, and aggregate changed-file/insertion/
deletion counts. Those facts identify tool and process failures without copying their
possibly private content. If the cause requires raw stderr, journal or diff contents,
the reviewer must inspect the disposable clone with authorization and arrange a new
private diagnostic run; the aggregate cannot establish a textual cause on its own.

The project `AGENTS.md` still instructs reviewers to inspect raw journal and streams;
its instruction must be reconciled by the lead before this aggregate can serve as
its sole acceptance evidence. This does not alter the separate `scripts/fanout.py`
p1 runner contract, frozen by `test_run_dir_layout` pending owner disposition.
