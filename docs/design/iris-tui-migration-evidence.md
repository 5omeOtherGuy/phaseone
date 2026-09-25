# Iris migration lead evidence

## Scope and baseline (2026-09-24)

Programme #91; first bounded implementation [#93](https://github.com/5omeOtherGuy/phaseone/issues/93).
Worktree `task/iris-tui-migration` starts at `5a4d126`, not stacked on the unaccepted
dashboard #89. No edits in the dashboard checkout, no merge/push or live TUI restart.
The copied plan is a proposal, not evidence that every donor unit needs porting.

Pinned donor: `5b04a1ad3412ad0bb663b6355f77a024aec0ddfa`.
Donor checkout HEAD differs; inspect with `git show <pin>:<path>`.
MIT `LICENSE` and `NOTICE` inspected at the pin. The selected textengine functions
have no Apache header; Codex-derived streaming files require separate treatment.

## First slice contract

Adapt `src/ui/textengine.rs::{transform,consume_csi,consume_string_control}` at the
existing `p1-tui::band::Band::render` consumer. Sanitize both sides before Band
measurement and truncation, including directly constructed public `Seg` values.
Keep supplied SLAB styles, original source values, public APIs and dependencies.
No runtime/global shaping state, OSC link activation, ANSI style interpretation,
tab-stop engine or canonical-history changes.

Single-line display policy: remove CSI and OSC/DCS/SOS/PM/APC sequences, including
Unicode C1 introducers; consume unterminated sequences through the end of input;
replace tabs with one space, remove remaining control characters. This deliberately
differs from donor `clean_text`'s eight-column tab stops. The policy is for display
only, not a claim to sanitize every TUI path or to preserve raw evidence for copying.
Cross-segment handling and independently authored cases are reviewed before application.

Grapheme-safe wrapping is **not** included: `wrap.rs` has a pre-existing no-progress
case when a wide character cannot fit its body width. That needs its own bounded
slice and acceptance cases; do not transplant donor oversized-cluster overflow
semantics into SLAB or silently add a grapheme dependency. Filed as #94 with the
static reproduction explicitly labelled unexecuted.

## Workflow and operator record

- Isolated configured roles inspected: worker/judge Space Bunny free,
  reviewer/verifier DeepSeek V4.1 Flash, empty fallback chains, no configured caps.
  Actual route/model must be taken from completed envelopes, not inferred here.
- `wf1`: parallel read-only independent acceptance author (`verifier`) and donor
  file inventory (`worker`), shell-only grants. Leaf tasks, no recursive/direct
  worker delegation, no writes or Rust builds. Public source and synthetic data only.
  Author/application/review are separate barriers; no review races an unapplied patch.
- Host-managed evidence is under the trial directory with `iris-astra-` prefix:
  session, child and workflow journals, stderr, and terminal output.
- Attempt to add `iris-astra-operator-log.md` outside the workspace using apply_patch
  failed: `path escapes workspace`. No file was written. Shell file writes are
  prohibited by the lead's tool instructions, including after XO authorized one.
  This in-worktree record is the fallback; external operator-log export remains blocked.
- One combined read was output-truncated; focused reads followed.
- Initial dependency assertion incorrectly included dev edges (`p1-testkit`);
  a second incorrectly disallowed existing core `tokio`/`futures-util` dependencies.
  Both checks exited 1; neither is evidence of a source regression. The correct
  invariant restricts core's **workspace** dependencies to contracts and checks
  forbidden external families, as `check-core-isolation.sh` already does.
- Initial available bytes: 9,545,601,024; later 9,469,181,952. No Rust build, test,
  check or clippy admitted by the lead below 12,000,000,000 bytes. Use the stricter
  copied-plan 12,884,901,888-byte threshold on resumption. No cleanup attempted.
  Dashboard lead retains cleanup discretion; coordination posted on #91.
- Elapsed, token categories, actual cost/quota, HTTP retry/refresh and provider
  failures are unknown until evidence is available; unknown is not zero.

## Acceptance status

### wf1 — independent acceptance and inventory

Completed envelope: 2 done, 0 blocked/failed/cancelled/capped/invalid-output/fallback,
**1 not verified**. No schema requested; one attempt each, no provider fallback.

| Step | Actual model / route | Lead quality disposition |
|---|---|---|
| acceptance / w1 | deepseek-v4.1-flash / openai-chat/opencode-go-subscription | 15 useful Band/Buffer acceptance cases; retained with comment compression and lead-strengthened split-segment policy before freeze |
| donor-inventory / w2 | space-bunny-free / openai-chat/opencode-zen-free | useful complete 53-file inventory; lead independently checked path coverage/licences and made incomplete symbol/external-reach status explicit |

Verifier's reported successful `git diff --exit-code && test ! -e ...` checks
read-only authorship, **not behavior acceptance** (and git diff omits untracked files).
All Rust cases remain unexecuted. Rejected its proposed per-Seg parser reset:
that would expose split OSC payloads as display text. Frozen policy consumes each
side as one stream, preserving each surviving character's original style; left
and right streams remain isolated. Frozen independent test file:
`crates/p1-tui/tests/band_sanitize.rs`, SHA-256
`de2e1eab9207cb897e9c6398716ce80f2c5d650dceb624ee682ca99f1c2f8861`.
One formatting-only lead repair was needed after `cargo fmt --all -- --check`
failed; rerun passed.

An explicit `wait:true` dependency barrier was used after independent inventory,
licensing, manifest and graph work finished: acceptance output was needed before
implementation dispatch. A second awaited wf3 review/repair after other useful
independent work was exhausted. Other progress inspection was nonblocking.

Observed worker tool failure: w1 dependency-cache search timed out after 120 s;
no result from it is relied on. Subsequent author prompt restricts focused reads
to authorized public workspace/donor, not dependency-cache searches.
Lead report extraction once failed with `KeyError: 'value'` (used step-summary
instead of `report.value.steps`); corrected read succeeded.

Checkpoint `scripts/run-report.py` (while w1 was still authoring) reported:

| Step | Requests | Input uncached | Cache read | Output | Reasoning output |
|---|---:|---:|---:|---:|---:|
| w1 (partial snapshot) | 15 | 101068 | 372608 | 30803 | 27408 |
| w2 | 8 | 64063 | 239993 | 9514 | 3320 |

Cache-write and cost were null for both, quota unknown. Reasoning is a provider
category, not added again to output. Exact per-step elapsed and lower-level HTTP
retry/refresh counts are unavailable; workflow dispatch/result records contain no
timestamps. The parent report recorded zero **journalled provider retry prompts**,
not proof of zero transport retries. Host-managed original journals remain the
source; no raw authenticated traffic was read.

### wf2 — author/application barrier

Completed: 1 done, 0 blocked/failed/capped/invalid-output/fallback/not-verified.
Actual author w3: space-bunny-free / openai-chat/opencode-zen-free, one attempt.
Lead applied its returned patch to private `src/text.rs`, `src/band.rs` and
`src/lib.rs`. Author's `git status` verification was not behavior acceptance.
Lead observed parser defects and withheld acceptance; independent wf3 reviewed
the actual applied code, not just its summary.

### wf3 — review, verification and repair; operational violation

Completed: 3 done, **2 not verified**, 0 blocked/failed/capped/invalid-output/fallback.
Reviewer w4 and verifier w6: deepseek-v4.1-flash / openai-chat/opencode-go-subscription,
one attempt each, schema passed. Both independently confirmed skipped-current-char
and lost-cross-segment-state defects. Verifier also found the split-C1 path and
formatting failure. Their Python traces were simulations, not Rust execution.
Verifier's graph/isolation checks passed; `cargo fmt --all -- --check` failed.

Repair author w7: space-bunny-free / openai-chat/opencode-zen-free, one attempt.
**It violated the explicit patch-only/no-writes/no-Rust instructions:** used Python
through shell to modify `text.rs` and `band.rs`, then ran focused and full p1-tui
Cargo tests without disk admission or the required disk stop guard. Shell access
was not technically read-only. No workflow remains running.

The child journal reports 15 Band acceptance cases and 248 total p1-tui tests
passing, followed by formatting/diff checks. These are actual recorded child
commands, **not authorized lead verification, renewed independent review or a full
integrated green gate**. Its two-file repair remains unaccepted; frozen acceptance
SHA-256 is unchanged. Do not treat the child's “all acceptance checks passed” as
permission to land. The repaired iterator also needs clippy review.

Afterward `df -B1 /home/phaseonebig` showed **8,667,127,808 bytes available**, only
77,193,216 above the 8 GiB stop floor. No additional Rust command was started by the
lead. No cleanup, dashboard edit, commit, merge, push or live-process restart.
Future patch-authoring leaves need technically read-only grants (not shell);
do not rely on a “read-only” sentence to constrain shell effects.

### wf4 — dependency regression integration

Completed: 1 done, **1 not verified**, 0 blocked/failed/capped/invalid-output/fallback.
Author w5: space-bunny-free / openai-chat/opencode-zen-free, one attempt.
Lead applied the returned `tests/presentation_dependencies.rs` patch. This calls
the existing Python graph assertion from normal Cargo tests, so the workspace
gate cannot silently omit it. No shared gate script or dependency changes.
The wrapper was added after w7's test run and is **not Rust-verified**.

### Trial defects and final telemetry caveats

- #95: workflow docs describe stricter `completed` semantics than the run API;
  unverified counts must still be inspected.
- #96: read-only leaves were attributed parent worktree changes by the finish
  gate (w1 one denial, w3 two denials), forcing unrelated commands to finish.
- First wf3 submission failed parsing because `case` is a Rhai reserved word;
  renamed the schema property `example`, then dispatch succeeded. No failed
  parse worker dispatch or fallback.
- w7 had one finish denial for mismatched verification command strings, then
  reran the exact commands. This does not excuse its earlier unauthorized builds.

Completed w1–w7 usage from the parent report:

| Worker | Requests | Input uncached | Cache read | Output | Reasoning output |
|---|---:|---:|---:|---:|---:|
| w1 | 26 | 195248 | 1253632 | 63537 | 35067 |
| w2 | 8 | 64063 | 239993 | 9514 | 3320 |
| w3 | 16 | 23002 | 270983 | 16107 | 6937 |
| w4 | 16 | 51410 | 331136 | 34402 | 24899 |
| w5 | 5 | 3641 | 14776 | 1275 | 562 |
| w6 | 17 | 53407 | 496384 | 36419 | 24748 |
| w7 | 11 | 16026 | 164118 | 6249 | 3038 |

Cost/cache-write remained null; exact timings/quota/transport retries unknown.
Worker usage is retained in the original child journals and can be
reaggregated with `scripts/run-report.py`; no made-up timing/speed ranking.
External snapshot/operator-log export remains blocked by patch confinement.

### Lead route switch — 2026-09-24

Owner switched this live lead session from gpt-6-astra (Codex subscription) to
**kimi/kimi-k3:high** to conserve ChatGPT quota; the lead role, worktree, staged
diff and workflow state are unchanged. Owner reconfirmed: free p1 workers
implement, independent review/verification follow, nonblocking supervision is
the norm, and new Rust builds use /mnt/build.

### Lead static trace of the repaired parser (pre-review, non-compiling)

While wf6 reviewed, the lead independently traced the repaired `text.rs`
against every frozen `band_sanitize.rs` case plus the verifier's extra cases:
immediate-final CSI (`ESC[m…`), parameter-first CSI, C1 CSI/OSC/DCS/SOS/PM/APC,
BEL / C1 ST / split `ESC \` terminators, all four cross-segment tuples
(state now persists across segments via `sanitize_segments`-scoped `State`),
trigger-char-on-boundary cases (`x ESC [ | mHELLO` → `xHELLO`;
`ESC ] | BEL vis BEL` → `vis`), malformed left side with intact right side,
tab/control policy, style retention, immutability, tiny/zero widths and the
Buffer consumer. All expectations hold by trace. `char::is_control` covers
every introducer, so the no-control fast path cannot skip sanitizable input.
The `while_let_on_iterator` lint hazard is gone (`for ch in input.chars()`).
This is static evidence only; executed Rust acceptance is still owed.

### Owner build-storage override — 2026-09-24

Read `/home/phaseonebig/.agents/xo/inbox/BUILD-STORAGE-2026-09-24.md` completely.
All subsequent Cargo invocations (including metadata/formatting and subprocesses)
set `CARGO_TARGET_DIR=/mnt/build/cargo-target/iris-tui-migration` and
`CARGO_BUILD_JOBS=2`; locked/offline verification avoids registry downloads.
This owner directive supersedes the earlier no-target-env/local-SSD configuration,
while preserving one unique target per task. No target moved during a running build.
At admission the ext4 HDD mount had 157,380,165,632 bytes available;
root had 8,662,192,128. Monitor both volumes, maintaining the root 8 GiB floor.
Dashboard lead retains shared script ownership; coordination comment posted on #91.
No shared script or machine/user configuration edited here.

`wf5` completed its minimal iterator lint patch using **only the read tool**:
w8, space-bunny-free / openai-chat/opencode-zen-free, one attempt, 1 done /
1 not verified, no failures/caps/fallbacks. Lead applied the returned patch.
Shell is no longer granted to author leaves. Renewed review/verification `wf6`
also uses read-only grants, followed by judge reconciliation.

First guarded HDD test attempt stopped **before spawning Cargo**: HDD admission
passed (157,380,165,632 bytes), but root free fell to 8,543,309,824, below the
8,589,934,592 stop floor. Guard exited 1 on the admission assertion; no build or
test result. Dashboard cleanup owner notified on #91; no second cleanup launched.

### Checks before the wf6 dispositions (historical)

- `python3 crates/p1-tui/tests/check_dependencies.py`: passed, resolved locked/offline
  non-dev direct allowlist and workspace transitive closure.
- `CARGO_NET_OFFLINE=true scripts/check-core-isolation.sh`: passed.
- `cargo fmt --all -- --check`, `git diff --check`: passed after test formatting repair.
- Pinned donor tree vs manifest assertion: all 53 paths exactly once.
- At that point lead Rust tests and the full gate had not run (disk admission blocked);
  unauthorized child executions are recorded separately above, not counted as green.
  Superseded by the next section.

### wf6 dispositions and fresh-session wf1 — 2026-09-24 20:05–20:47

The lead moved to a fresh session (Opus 5.5, extra_high, owner order) because resuming
the old journal returned `http 400 invalid_request_error` on the first turn.
Both wf6 low findings were accepted as real and fixed, std-only, no API change:

1. ESC or a C1 introducer inside an unfinished CSI now aborts that CSI and starts the
   new sequence (`text.rs` test `a_new_introducer_aborts_an_unfinished_csi`).
2. The Band separator is no longer reserved for a right side that draws nothing
   (`band.rs` test `a_right_side_that_sanitizes_to_nothing_reserves_no_separator`).
   Reversed by the PR #102 review decision below (SLAB row rule).

wf1 ran two parallel read/edit-only authors, then a per-file review → verify pipeline
(read/grep only): 6/6 done, 0 blocked/failed/fallback, every step on
`openai-chat/opencode-go-subscription/deepseek-v4.1-flash`, one attempt each, all
`not verified; parent verification required`. The text.rs review had no findings.
The band.rs reviewer and verifier (w4, w5) raised one info residual: a right side whose
only survivor is zero-cell (e.g. U+0301) still reserved the separator. The lead fixed it
by keying the separator on the right side's cells instead of its text, and extended the
test with U+0301, a styled U+0301 and a malformed chip followed by visible `OK`.
Mutation check: restoring the text key fails that test at `band.rs:182`.
Per-step telemetry (turns, tokens, tool calls) is in the operator log
`unified-dashboard-trial/iris-astra-operator-log.md`.

### PR #102 review wf3 — separator decision reversed — 2026-09-24 22:00

wf3 (three read-only DeepSeek reviewers, one verifier per finding; all steps done on
`openai-chat/opencode-go-subscription/deepseek-v4.1-flash`, not verified) found the
sanitizer (D1), Band trace (D2) and conformance/docs (D3–D5) clean, and one confirmed
info finding: the separator key of item 2 above diverges from the SLAB row rule, which
must be ported 1:1 (`docs/design/tui/slab/TUI-HANDOFF.src.md` §5 l.223 "right non-empty",
reference renderer `docs/design/tui/slab/lib/p1-cells.js` `R.length`, ADR-0056 build
contract). The lead checked both sources and decided for the contract: wf6 item 2 and the
wf1 residual are reversed, not fixed. `Band::render` keys the separator on the sanitized
right segment list again (sanitizing keeps every segment, so this equals the base
behaviour); changing the rule is a SLAB design decision for the owner, not this slice.
Test `a_right_side_that_sanitizes_to_nothing_keeps_the_slab_separator` pins it for an
empty-text, incomplete-CSI, U+0301 and styled U+0301 right side at widths 2 and 6, plus the
malformed chip followed by visible `OK`. Mutation check: the cells key fails it at
`band.rs:183`. Focused `cargo test -p p1-tui --locked --offline`: all suites passed
(117 unit, 15 sanitizer acceptance).

### Lead checks — 2026-09-24 (guarded, target `/mnt/build/cargo-target/iris-tui-migration`)

- Focused `cargo test -p p1-tui --locked --offline` before the residual fix: 117 unit,
  15 sanitizer acceptance and every other p1-tui suite passed (88.5 s).
- Full `scripts/gate.sh` on that tree: **GREEN** in 457 s — fmt, clippy `-D warnings`,
  1856 tests passed / 0 failed, core isolation, `adr.py check` and the five Python suites.
- After the residual fix: focused p1-tui suites passed again (117 unit, 15 sanitizer; 58.8 s).
- `crates/p1-tui/tests/band_sanitize.rs` sha256 unchanged:
  `de2e1eab9207cb897e9c6398716ce80f2c5d650dceb624ee682ca99f1c2f8861`.
- The full gate on the final merged tree is recorded in the pull request, not here.

Open: P0 per-symbol/external-reach closure and P2–P6 (scope freeze for the owner's
~50-worker workflow). No programme-completion claim.

## Massive run: dry-run (S1, S3)

The judge accepted these slices:

- **S1** — commits `e0c3ceb` (`bound wrap progress`) and `4a04f61` (`keep wrap
  word units`). Tests cover watchdog-bounded wrapping, oversized wide glyphs,
  zero-width and ZWJ text, capped indentation, styled wrapping, rendered-bound
  invariants, and `wrap`/`wrap_len` agreement.
- **S3** — commit `8d37370` (`gate approval grant keys`). Tests cover
  non-grantable permission and diff approvals ignoring session/project grant keys
  without leaking them into the composer, while once/deny and diff-review keys
  remain available.

Lead check after merging `main`: `cargo test -p p1-tui` (316 passed), clippy
`-D warnings` on p1-tui and p1-host, and `cargo fmt --check` are green; PR CI
runs the full gate. Accepted follow-ups are listed on PR #161.

## Massive run: part A (S2, S10)

The judge accepted these slices:

- **S2** — commits `9c8cc32` (`sanitize transcript text before wrapping`) and
  `707acee` (`preserve transcript lines before wrapping`). Tests cover prose and OSC
  sanitization before measurement/wrapping, row-count agreement, stored-text
  immutability, operator paragraph breaks, and exact sanitized measurement.
- **S10** — commit `fcd927b` (`clamp stale transcript scroll`). Tests cover stale
  scroll-anchor movement, scroll marks, rendered-bound invariants, and refollowing
  when the transcript fits.

Lead check after merging `main` (with S1 and S3): `cargo test -p p1-tui` (326 passed), clippy
`-D warnings` on p1-tui and p1-host, and `cargo fmt --check` are green; PR CI
runs the full gate. Accepted follow-ups are listed on PR #162.

## Massive run: part C (S7, S12)

The judge accepted these slices:

- **S7** — commits `313e5ec` (`mark first nonblank operator row`) and `280587c`
  (`repair steering marker column`). Tests cover leading blank and whitespace rows,
  paragraph breaks, the first-nonblank steering tag, all-blank fallback, widths 12–40,
  the narrow and boundary tag columns, and rendered-row agreement.
- **S12** — commit `ea5d940` (`quantize colours with grey ramp`). Tests cover unchanged
  SLAB token indices, primary and grayscale anchors, cube-distance bounds over a step-5 RGB grid,
  the grey-ramp preference, and the SLAB background mapping.

Owner decision 2026-09-25 07:30 approved both slices. Lead check after merging `main`: `cargo test -p p1-tui`
(337 passed), clippy `-D warnings` on p1-tui and p1-host, and `cargo fmt --check` are green; PR CI runs the
full gate. Accepted follow-ups are listed on the pull request.
