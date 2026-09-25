# BLOCK implementation and verification

Implemented in `task/block-implementation`, isolated from Kimi's active checkout. No commits or merges were made. The supplied `Downloads/p1-block-spec/BLOCK-SPEC.md` and adopted screen 3b are the visual reference for this increment.

## Try it

From this worktree, `cargo run -p p1-host --bin p1 -- --tui` runs the real application with the existing configuration. The built executable is `target/debug/p1`; the globally installed command has not been replaced.

For a repeatable demonstration without network access or credentials:

```sh
P1_FIXTURE_KEY=fixture cargo run -p p1-host --example tui_fixture -- /tmp/p1-block-demo
```

This uses the real host, provider adapter, journal, terminal driver and the shell, read, write and edit tools with a deterministic in-process provider transport. The reply is chosen by a keyword in the prompt (see the example's doc comment): none runs a real shell command producing 60 rows; `stream`, `slow` and `md` stream prose; `long` prints 4000 rows; `fail`, `wide`, `files`, `many`, `flood N`, `cancel` and `error` cover failures, wide output, file tools, several calls, large histories, cancellation and a provider error. `P1_FIXTURE_ASK=1` runs under `--ask`. Starting again on the same directory resumes its journal. It is a test fixture, not a connection to the displayed model.

`.verification/finish/tui.py` drives either binary in a real pty (tmux) or in Ghostty (xdotool) for scripted checks and screenshots.

## Everyday controls

| Action | Control |
| --- | --- |
| Submit | Enter |
| Insert newline | Shift+Enter; Ctrl+J fallback (the hint row names the one that works) |
| Paste | Terminal paste: inserted atomically, escape codes stripped; never submits, never decides an approval, never dropped |
| Move within input | Arrows (Up/Down by painted row), Home/End, Ctrl+A/E, Alt+←/→, Ctrl+←/→, click |
| Erase input | Backspace/Delete, Ctrl+U/K, Alt+Backspace, Alt+D, Ctrl+Backspace |
| Yank / undo | Ctrl+Y inserts the last erase; Ctrl+Z undoes |
| Recall prompts | Up/Down at the first/last row; any edit keeps the draft; steering and follow-ups are recalled too |
| Commands | `/` opens the palette: ↑↓ select, Tab completes, Enter runs; commands run locally even during a turn; a one-line `//x` sends `/x` |
| Clear / quit | Ctrl+C: closes an overlay, then clears a draft (Up brings it back; while browsing history it returns to the unsent draft), then quits; right after a turn ends it asks for a second press (`^C again to quit` / `to clear`) |
| Cancel running turn | Ctrl+C (also when typed right after the Enter that started it): queued steering, follow-ups and an unsent prompt return to the composer in the order typed |
| Queue follow-up during a turn | Alt+Enter; Enter queues steering |
| Expand / fold a tool output | Click its header, or Tab to select blocks, ↑↓, Enter |
| Open an output in the pane | Click its fold row, `o` on a selected block, Ctrl+O (the fold row on screen, else the newest folded output), `/outputs`, `/open h-xxxx` |
| Copy | `/copy` (last reply), `y` in the output pane (as filtered) or on a selected block, `/copy h-xxxx` |
| Output pane | ↑↓ PgUp/PgDn Home/End scroll, ←→ pan, `/` filter (Esc restores), typing returns to the draft, Esc clears a kept filter then closes, Ctrl+O opens the newer output its header names (`newer h-xxxx ^O`) |
| Scroll transcript | Wheel, PageUp/PageDown, Alt+↑/↓, Ctrl+Home / Ctrl+End (oldest / follow live) |
| Search transcript | `/find text`; again for older matches (new ones count); the highlight follows its block through resizes and toggles |
| Terminal selection | Shift+drag, or `/mouse` to release the mouse entirely |
| Pane | Ctrl+W width, Ctrl+Tab or F6 mode, `/pane ledger\|output\|diff\|workers`, Ctrl+P pin |
| Approvals (`--ask`) | `y` allow once, `a` session, `n` deny, alone after a pause (`y⏎` confirms at once, Esc takes the key back); keys typed as it appears go to the draft; nothing opens over it |
| Overlays | `/help`, `/status`, pickers and the palette dock above the composer; ↑↓ or the wheel scroll or select; Esc closes |
| Discover controls | `/help` |

Shift+Enter was verified in Ghostty 1.3.1. Legacy XFCE terminal through tmux did not distinguish it from Enter in the tested configuration; the UI detects missing enhanced-keyboard support and advertises Ctrl+J. Bracketed paste prevents pasted newlines from submitting prompts and pasted decision keys from approving tools.

## Design and performance

The production renderer uses full-width background bands, exact cell padding, one-line outcome-first headers, hard-truncated tool rows, restrained diff colors, short output handles, a transcript-width composer, and an inset side pane. Narrow output views become opaque overlays. Truecolor, indexed color, plain output and reduced motion are covered by tests. The 120-column layout has 74 transcript cells: 120 minus two outer insets of two, the two-cell gutter and 40-cell pane. This resolves the supplied document's inconsistent width arithmetic in favor of its explicit insets.

Iris was reviewed as a read-only donor, especially its transcript, screen, rows, pager, text, clipboard, steering, slash and streaming code. The adaptation does not transplant Iris's application state or runtime. See the finishing-pass section below for the layout, input and frame-pacing mechanics taken from it.

Kimi's useful fixes for monotonic event timestamps and restoring pane width after promotion were retained. The older whole-transcript rendering path was replaced in production. Compatibility APIs remain for existing callers.

## Verification and evidence

The first increment's evidence is `.verification/index.html` (screenshot gallery), `.verification/gate.log` and the mouse section below. The finishing pass's evidence is under `.verification/finish/` (see below).

## Explicit boundaries

The renderer supports all twelve supplied tool shapes. Native tools use their actual input/results; richer ask/delegate/notify fixtures demonstrate optional structured payloads. This increment does not add missing agent tools or change their protocols. Missing cost, context or metadata stays unknown. Settled edit results cannot reconstruct file line numbers the tool did not record; their number field stays blank, while approval diffs use the before-image.

Four-digit handles are reconstructed deterministically from journaled call IDs with collision probing. Existing eight-digit aliases remain usable. No new journal record type was introduced.

## Mouse interaction verification — 2026-09-21

Mouse capture is enabled for the alternate-screen session and restored on exit.
Click a tool header (including its disclosure arrow) to expand the complete
inline output; click again to fold it back. Each tool retains its own state
across redraws and terminal resizes. The initial state remains the design's
compact output preview.

Adapted interaction mechanics from the read-only Iris donor:
`src/ui/tui_loop.rs` (`header_click`, `pager_wheel`) and
`src/ui/tui/pager.rs` (`ScrollState`).

## Finishing pass — 2026-09-22

A critical review (five code reviewers, three real-terminal testers, two Iris
studies: 143 findings, nearly all reproduced) drove this pass. Discovery
evidence: `.verification/finish/evidence/{r1..r5,t1..t3,d1,d2}-*`.

**Layout and scrolling.** Every block's height is measured cheaply and kept;
styled rows are built only for blocks a frame shows. Tool bodies never wrap, so
a width change re-measures prose only. A detached view is held as (block, row
offset) and re-resolved every frame, so a resize, a pane toggle, `^R` or an
expanded block above never moves the reader's content; reaching the bottom
re-follows. Clicking a header toggles what is shown (a folded preview expands
and returns; a body that fits hides and returns) without detaching a view that
follows the live tail. Fold rows open their own output; reasoning rows toggle.

**Prose.** Tabs and controls are cleaned before wrapping (nothing is cut
behind a `›`), interior spacing is kept, list items hang under their text,
fenced code keeps its lines, blank runs collapse, and a block never starts or
ends blank, so there are never two blank rows. Failures carry a `✗`.

**Working and cancel.** The working state is a GROUND row `▪▪▪ waiting for the
model / thinking / writing` (screens.html 2b), not a fake tool block. Reasoning
shows its duration. A cancelled turn is marked `· cancelled after 3.2s`; queued
steering is taken back from the agent (`Inbox::withdraw`, ADR-0047) and returns
to the composer with follow-ups and an unsent prompt, so no turn starts after
Ctrl+C. Denied, unavailable and never-started calls get their row live.

**Composer.** A text model (`crates/p1-tui/src/editor.rs`) shared by painting
and the caret: word wrap with a glyph fallback, Up/Down by painted row with a
sticky column, grapheme-cluster editing, kill/yank, undo, click-to-caret,
windowed layout for large drafts, and `↑ N ↓ M more` when the draft overflows.
History browsing ends on any edit, so drafts and edits are never replaced.

**Approvals.** The pending call is the transcript's last block (screens.html
2c) with its decision row pinned under it; the prompt, reasoning and draft stay
visible, and the pane is hidden. A decision key counts only after the approval
has been visible 400 ms and after a 300 ms pause in typing; keys typed as it
appears go to the draft. Rows state facts: the real tool name, `network on`
(neither sandbox mode isolates it), writes over existing files show what they
remove, patches show their hunks, `p project` is shown unavailable until a
trust store exists. Decision rows wrap rather than clip.

**Frames.** Redraw is demand-driven (Iris RenderScheduler): at most one frame
per 16 ms, none when nothing changed; the working LEDs tick every 50 ms and only
while visible; ready input is drained before one draw, wheel bursts coalesce,
mouse motion costs nothing, a width change settles 50 ms, frames are written
once through a buffer inside a synchronized update. SIGTERM/SIGHUP end the
session cleanly and a panic hook restores the terminal even under the release
profile's `panic = "abort"`.

**Resume.** A resumed session is painted from its journal records: partial
replies with their cancel mark, provider failures, steering, notifications,
compaction markers, spend summed from recorded usage and the task diff from the
recorded edits. Calls left without a result settle as unknown.
