# Independent review of the first slice (2026-09-20) — dispositions

Review artefacts (kept unchanged): `../phaseone-review-2026-09-20/` — `REVIEW.md`,
`reproductions.patch`, logs. Reviewed commit `736cd71`. The reviewer's plan amendments:
`../phaseone-briefs/plan-amendments-from-reviewer-2026-09-20.md`.

Rules used here. "Already known" is not a disposition. A broken guarantee is fixed, never
re-described as intended by an ADR. The reviewer's five reproduction tests were applied
unchanged (only `rustfmt`), confirmed red against `4a0deaa` for the stated reasons
(commit `cf27202`), and are now part of the suites — their assertions were not touched.

## Findings requiring fixes

| # | Finding | Valid? | Severity | Known before the review? | Disposition | Fixing commit | Tests that now hold it |
|---|---|---|---|---|---|---|---|
| R1 | Cancel/timeout can leave a TERM-ignoring descendant alive inside the shell's own process group | Yes — reproduced | P1 | No. The report disclosed a different limit (processes that ESCAPE the group) | Fixed: `terminate` watches the GROUP, not the shell: TERM → grace → KILL what is left → return when the group is empty | `dd48b77` | reviewer `review_cancel_kills_term_ignoring_descendant`; lead `lead_group_cleanup.rs` (cooperative descendants end fast; timeout path too) |
| R2 | Simultaneous credential refresh can deadlock the current-thread runtime | Yes — the reviewer found it by inspection; the lead then REPRODUCED it (the new test hangs on the old code) | P1 | Partly, and misjudged: the report listed "blocking file lock on the runtime thread" as latency debt. The consequence — deadlock, with cancellation and Ctrl-C dead too — was not recognised | Fixed: `p1-provider-http::lock_exclusive` (`try_lock` + async pause, 120 s patience, drop-safe) in both adapters; the second source re-reads under the lock, so there is exactly one rotation | `0872eb8` | `file_lock` unit tests; `lead_refresh_contention.rs` in both adapters (two sources, one lock, first refresh held pending) |
| R3 | An idle interactive parent does not wake when its worker finishes | Yes — reproduced | P1 | No. The report said only that nobody had typed into the interactive loop. The delegation test `child_finishing_while_parent_idle_wakes_it` proved the core mechanism by performing the host's integration step itself | Fixed: the prompt wait also selects on `inbox_ready()` and runs the inbox turns; the pending line read is dropped for that, so the line source became cancel-safe (typed bytes live in the source) | `7cb79a8` | reviewer `review_interactive_idle_parent_wakes_on_child_completion`; lead `lead_line_source.rs` |
| R4 | `worker_continue` bypasses `max_concurrent` | Yes — reproduced | P2 | No | Fixed together with R5: every transition into Running goes through one critical section that checks the limit | `dd48b77` | reviewer `review_continuation_obeys_global_limit` |
| R5 | A cancel right after a continuation is lost | Yes — reproduced | P1 | No | Fixed: whoever sets a child Running (`start`, `continue_child`) installs that turn's cancellation token under the same lock, before the task can see the turn; `shutdown` covers accepted-but-unpolled turns the same way | `dd48b77` | reviewer `review_cancel_immediately_after_continue_is_retained` |
| R6 | Tail repair truncates a journal an active writer has locked; the host loads and repairs before owning the file; `open_for_append` trusts a caller-supplied sequence number | Yes — reproduced | P1 | No. `journal.md` promised "a lock file makes a second writer fail fast" — that promise was broken for repair | Fixed: `JsonlJournal::resume` locks FIRST, then reads, validates, repairs and derives the sequence under that lock, held for the writer's life; the host uses it. `repair_truncated_tail` locks and rejects a stale observation (`StaleTail`); `open_for_append` locks before reading and rejects a stale sequence number | `7cb79a8` | reviewer `review_repair_respects_active_writer_lock`; lead `lead_ownership.rs` (5 tests); the every-byte crash-cut test still passes |
| R7 | The CI run of the final report commit was red (`a_timeout_kills_the_whole_group`: bash start-up raced a 1 s timeout) | Yes | P2 | No — and the slice report said "CI green". That statement was wrong: the lead checked the previous run, not the last one | Fixed without weakening the assertion: the timeout is an injected future that fires once the pid is published; the group-cleanup assertion is unchanged; a separate test covers the real clock. Process change: the lead reads the CI result of the FINAL commit before reporting | `dd48b77` | `a_timeout_kills_the_whole_group`, `timeout_seconds_stops_a_long_command` |

## Design departures the review asks to make explicit

| # | Departure | Valid? | Disposition | Where it is tracked |
|---|---|---|---|---|
| D-A | Shared workspace ownership weakened: `seams.md §8` requires conflicting parallel writes to be serialized or isolated; `delegation.md` made it "the model's responsibility" | Yes. The detailed spec said so openly, but it is a weakening of the baseline, not a refinement | DONE (`09f44b2`, ADR-0032): file-tool mutations of all agents of one process are serialized by a shared write gate; the stress test loses an update without it. Shell commands are NOT covered and the docs say so; isolation per child stays future work. Per-path grants are not claimed to constrain shell commands without an execution boundary | issue #1 (closed) |
| D-B | Changed-route resume warns, drops foreign reasoning replay and proceeds; `seams.md §3` requires translation or explicit rejection | Yes | DONE (ADR-0033): the core rejects a resume whose origin (route or model) differs, before anything is committed | issue #2 |
| D-C | Model adaptation stops before policy (same passthrough context policy and authorization for every model) | Yes, as a statement of scope | Accepted as scope of the slice. Context control arrives as an injected policy (plan step 3) with cancellation and restart tests | plan step 3 |
| D-D | Resume is not restartable orchestration: child sessions, worker ids and retained repair state do not survive the process | Yes — deferred capability, not a defect of ordinary resume | DONE (ADR-0034): on resume the user and the model are told which workers are gone; their ids are never reused (they restarted at `w1` before — a new worker would have answered to an old name). Persistent child lifecycle only after longer sessions succeed | issue #3 |
| D-E | One shell command is capped at 3,600 s | Yes — disclosed in the tool schema; not a hidden limit | Deferred deliberately: decide when long builds/jobs become a target. No change now | this table |

## Remarks on the evidence — accepted

- 554 is a count of test functions, not of independent guarantees; suites that share a spec
  share its blind spots (R3 is the example). Consequence: lifecycle behaviour is tested at
  the HOST, end to end, not only in the layer that provides the mechanism.
- "0 failed tool calls" does not mean every command succeeded: a non-zero shell exit is
  `ToolStatus::Ok` by design. Run records will count non-zero exits separately.
- Footprint and cost figures are reported observations, not reproduced measurements;
  future numbers come with a workload definition and parent-plus-descendant accounting.
  The $4.58 worker spend excludes the lead session and subscription usage.
- `read` loads the whole file before paging, and histories/retained workers grow: low
  idle RSS is not bounded long-run memory. Tracked for the measurement step.
