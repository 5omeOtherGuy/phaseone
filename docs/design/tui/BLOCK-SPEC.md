# BLOCK — transcript rendering spec

Implementable specification for the p1 TUI transcript. Companion to `preview.html`
(open in a browser; ten slides, arrow keys to navigate).

This document supersedes §3 and §4.3 of `SPEC.md` for how a **tool result** is drawn.
Everything else in `SPEC.md` (palette, symbol vocabulary, right pane, 80-column floor,
keybindings, non-goals) still holds and is not restated here except where BLOCK pins it.

Target: 120×40 typical, **80×24 floor**. Dark ground only. Truecolor terminal assumed;
degrade per §9.

---

## 1. The model

A transcript is a vertical list of **events**. There are exactly three event shapes:

| Shape | Ground | Chrome |
|---|---|---|
| operator input | GROUND `#0a0a0a` | none — `› ` prefix, one blank line after |
| assistant prose | GROUND `#0a0a0a` | none — no gutter, no role label, no bubble |
| tool event | BLOCK+ / BLOCK | the three-band block (§2) |

**Chrome is earned.** Only a tool event gets a background. Nothing else in the
transcript ever draws one.

---

## 2. The three-band block

```
┌ band A ─ header ── background #1c1c1c ── exactly 1 row ───────────────┐
│ ▸ shell     cargo test -p brain note::            ✗ 11.4s · exit 101  │
├ band B ─ body ──── background #121212 ── 1..N rows ───────────────────┤
│ ---- note::roundtrip stdout ----                                      │
│ assertion `left == right` failed                                      │
│    left: "s-2026-09-19-a"                                             │
├ band C ─ meta ──── background #121212 ── 0 or 1 row ──────────────────┤
│ · 86 more lines folded → [h-2b14]                     ^O open in pane │
└───────────────────────────────────────────────────────────────────────┘
```

The box above is documentation only. **No box-drawing characters are ever emitted.**
The bands are told apart by their background fill and nothing else.

### Geometry (character cells)

Let `W` = transcript width in columns (terminal width minus the right pane, §5 of SPEC.md).

- Every band is filled edge to edge across `W`. Trailing cells are painted with the band
  background, not left transparent — a ragged right edge is the one thing that makes the
  block look like a box that failed.
- Horizontal padding inside every band: **2 columns** left, **2 columns** right.
  Usable width `U = W - 4`.
- Vertical padding: **0 rows**. Bands are flush; the background step is the separation.
- Between one event and the next: **1 blank row on GROUND**. Never 2, never 0.
- A block is never indented. Delegation content indents *inside* band B (§4.6).

### Band A — header, always exactly one row

```
<glyph><space><name padded to 10><argument>…<gap>…<outcome>
 1      1      10                  flex           right-aligned
```

- Columns 1–2: the glyph and one space. Glyph set is fixed (SPEC.md §2).
- Columns 3–12: the tool name, lowercase, left-aligned, space-padded to **10**.
  The name field plus the leading glyph and space is the 12-column field from SPEC.md §3.
  Names longer than 10 truncate to 9 + `…`.
- Column 13 onward: the argument, at INK.
- The **outcome** is right-aligned to column `U`. It is one string: `✓`/`✗` plus the
  facts that belong to the call as a whole (elapsed, line count, byte count, exit code,
  file count). Separator between facts is ` · `.
- If argument and outcome would collide, the **argument** truncates with `…`; the outcome
  is never truncated and never wraps. Minimum gap between them: 2 columns.
- Colour: glyph per SPEC.md §2 · name DIM · argument INK · outcome DIM, except the
  `✗` marker itself which is INK.

### Band B — body, the output verbatim

- The tool's own bytes, unmodified except for: tab expansion (8), control-character
  stripping, and truncation at `U` columns with a FAINT `›` in the last cell to mark a
  cut line. **Body text never soft-wraps.** A wrapped shell line is unreadable and breaks
  every column the tool itself aligned.
- Default colour DIM `#9a9a9a`. The body is not INK: it is reference material, not the
  answer. Exceptions are the diff hues (§4.4) and nothing else.
- Line numbers, where the tool has them, occupy a right-aligned field of
  `max(3, digits)` columns at FAINT, then 2 spaces, then the content.
- Band B is omitted entirely only when the tool genuinely produced no output
  (`stop`, `search` with 0 hits). In that case band C carries the one-line result.

### Band C — meta, zero or one row

Present when any of these are true, in this precedence order — only the first applies:

1. output was folded → `· N more lines folded → [h-xxxx]` at FAINT, with the key hint
   `^O open in pane` right-aligned at FAINT;
2. the call has facts that did not fit band A → e.g. `exit 0 · 6 lines · cwd ~/brain-tools`
   at DIM, left-aligned, no right column;
3. the event is blocking → the decision row (§3), which is on **BLOCK+**, not BLOCK.

---

## 3. Decision rows

A blocking event (approval, question, worker review) replaces band C with a decision row
drawn on **BLOCK+ `#1c1c1c`**, so the block reads header–body–header.

```
 y  allow once   a  session   p  project   n  deny        ^D next file   ^A all files
```

- Each key is one character rendered **inverted**: background INK, foreground GROUND,
  with one padding space each side (` y `). Inversion is the only highlight in the
  interface and is used here and for the focused picker row, nowhere else.
- Label after each key at INK, 1 space gap, 3 spaces between pairs.
- Secondary keys right-align at FAINT.
- An ungrantable decision keeps its key visible but renders key and label at FAINT with
  the reason inline: ` a  session      not grantable — destructive floor`.
- Never a button row, never a mouse target, never a `[Y/n]` prompt.

---

## 4. Per-tool rendering

The renderer dispatches on tool name. Everything below is band A argument + band B body.

### 4.1 read

- argument: path, relative to cwd. Outcome: `✓ N lines · S kB`.
- body: the requested range with FAINT line numbers. Non-contiguous ranges are listed in
  order with no separator row — the line numbers already say there is a gap.
- fold: bodies over **40 rows** keep the first 24 and fold the rest.

### 4.2 shell

- argument: the command, single line. A multi-line or heredoc command is replaced by its
  `description` when one exists; otherwise the first line plus `…`.
- outcome: `✓ 11.2s` on exit 0; `✗ 11.4s · exit N` otherwise.
- body: stdout then stderr, in that order, no marker between them. Colour DIM for both —
  stderr is not INK, and it is never a hue.
- band C: `exit 0 · N lines · cwd <path>` when nothing was folded.
- fold: over **40 rows**, keep the **last** 24 (a test run's verdict is at the bottom).

### 4.3 write

- argument: path. Outcome: `✓ N lines new` or `✓ N lines replaced`.
- body: the first 6 lines of the written file as an add-diff (DIFF-ADD hue, `+` column),
  then fold.

### 4.4 edit — diff body

- argument: path. Outcome: `+A −B · i of N files`.
- body: 1 row of context, all removals, all additions, 1 row of context. Removals before
  additions; never interleaved.
- Each diff row is `<lineno FAINT><2sp><+|-|space><1sp><content>` filled to `U` with the
  hue background.
  - removed: bg `#4a3535`, fg `#e8d0d0`
  - added: bg `#3a4a3a`, fg `#d8e8d0`
  - context: band B background, content DIM, line number FAINT
- The `+`/`−` column is mandatory. Strip the hues and the diff must still read.
- These two hues appear in diff bodies and nowhere else in the product — not in status,
  not in emphasis, not in a worker row.
- Blocking edits hide the right pane and take full width (SPEC.md §4.4).

### 4.5 skill / search / send / stop — thin tools

Thin tools produce a header and a 1–3 row body. They still get a block; a bare line
would make them look like assistant prose.

```
▸ skill     model-cards                                      ✓ loaded
  3 cards · deepseek-v4.1-flash · kimi-k3 · glm-5.3
  evidence.jsonl 41 rows · last accepted 2026-09-19
```

- `search`: body lists each hit as `name — one-line purpose`, max 8, then `· N more`.
- `send`: body is the summary line plus the defect/instruction list, max 6 rows, then fold.
- `stop`: no body; band C carries `local agent · N calls spent · worktree kept`.

### 4.6 delegate

- argument: `N workers` (or the single worker's task description). Outcome: `ceiling N calls`.
- body: one three-row group per worker, separated by a blank row:

```
▪ s1-iris         deepseek · v4.1-flash              1m12s · $0.02
  owns  spikes/s1-iris/** · docs/spikes/S1-iris-as-lib.md
  ↳ cargo fetch iris_agent v0.4 — resolving 38 crates
```

- Row 1: glyph, name at INK, `route · profile` at DIM, elapsed and cost right-aligned.
- Row 2: `owns` + the worker's owned paths. Mandatory — a worktree collision must be
  visible before the merge, not after.
- Row 3: the current activity, prefixed `↳`. **Indent is 2 columns and never deepens.**
- Ordering: `!` needs review, `▪` running, `✓` done, `·` queued.
- A worker awaiting review carries its decision row: ` r ` review / ` y ` apply /
  ` n ` discard worktree.
- Unknown cost renders `—`, never `0`.

### 4.7 ask

- argument: the question, one line. Outcome: `pick one` or `pick any`.
- body: one row per option — `<glyph> <label padded to 22><description DIM>`.
  The focused row is fully inverted across `U`. Unfocused rows are prefixed `·` at FAINT.
- band C: what the answer will change, at DIM, plus `space toggle   ⏎ confirm   esc dismiss`
  at FAINT, right-aligned.

### 4.8 notify — a worker returning

Rendered as its own block, not as an update to the delegate block above it (the transcript
is append-only; nothing already drawn is rewritten).

```
▸ notify    s1-iris returned                            ✓ 4m02s · $0.06
  verdict  iris_agent builds as a lib; 2 pub items missing
  files    3 written, all inside owned paths
  ↳ review staged worktree before merge
```

Label column inside the body is 9 columns, DIM; values INK.

### 4.9 compact

```
▸ compact   second-brain scope only                       ✓ 128k → 14.2k
  kept     design decisions · open spikes · handoff contract
  dropped  6 sidequests, named but not summarised
  · full summary retained → [h-0c8e]                              ^O open
```

---

## 5. Folding

- Threshold: **40 body rows**. Below it, nothing folds — expanded is the default and the
  operator must never have to press a key to see a result they are being asked to judge.
- Kept rows: 24. Head for `read`/`write`/`edit`, tail for `shell`, head for everything else.
- The fold handle `[h-xxxx]` is a stable 4-hex id, unique per session, addressable:
  `^O` opens **that** object in the OUTPUT pane; `/open h-2b14` does the same from the
  composer. The id survives scrollback and is written into the journal.
- A folded block never re-expands in place. It opens in the pane. The transcript is
  append-only and its row count must be stable under scroll.

---

## 6. Colour and contrast — enforced

| Token | Hex | Where |
|---|---|---|
| GROUND | `#0a0a0a` | input, prose |
| BLOCK | `#121212` | band B, band C |
| BLOCK+ | `#1c1c1c` | band A, decision rows |
| RULE | `#2a2a2a` | unfilled bar segments only |
| INK | `#e8e8e8` | arguments, values, active glyphs, decision labels |
| DIM | `#9a9a9a` | tool names, body text, labels, outcomes |
| FAINT | `#6a6a6a` | line numbers, fold metadata, key hints, timestamps, queued/unavailable rows |
| DIFF-ADD | `#3a4a3a` / `#d8e8d0` | added diff rows |
| DIFF-DEL | `#4a3535` / `#e8d0d0` | removed diff rows |

**The FAINT rule is a test, not a preference.** FAINT is 3.4:1 on BLOCK. A tool outcome,
a provider name, a file path, a token count, an error message is never FAINT. If a string
must be read to make a decision, it is DIM or INK.

Sibling rows share one colour. Never one worker at INK and its neighbour at DIM.

---

## 7. Motion

- Working indicator: three `▪` cells, 1.1s cycle, 0.18s stagger, opacity 0.18 → 1.0.
- It lives on the header row of the running block, in the outcome position, and is the
  only animated thing on screen.
- `P1_REDUCED_MOTION=1` freezes it to a static `▪▪▪`.
- No braille spinners, no progress animation on a block that is not running, no
  re-render of settled rows.

---

## 8. 80-column floor

The block keeps its shape: same three bands, same 12-column name field, same 2-column
padding, same indentation and hues. `U` simply shrinks and arguments truncate earlier.
The right pane collapses; one DIM line appears under the composer:

```
ask · fable · 12.4k                              ^L ledger   ^C cancel
```

That is the only bottom-of-screen state in the design.

---

## 9. Degradation

| Capability | Behaviour |
|---|---|
| 256-colour | nearest-cube approximations of the nine tokens; hue pairs must stay distinguishable from BLOCK |
| 16-colour / no truecolor | bands drop to default background; blocks are then separated by one blank row and the glyph column alone. Never substitute box-drawing to compensate. |
| `NO_COLOR=1` | all nine tokens collapse to default fg/bg. The screen must remain fully readable — this is the acceptance test for §6, not a fallback |
| narrower than 80 | same rules, `U` shrinks; no reflow into a second shape |

---

## 10. Acceptance checks

A build is correct when all of these hold:

1. `NO_COLOR=1` — every state in the transcript is still identifiable from glyphs,
   the `+`/`−` column and layout alone.
2. Grep the renderer for box-drawing codepoints (`U+2500`–`U+257F`): zero hits.
3. Every band fills to the full transcript width; no ragged right edge at any width
   between 80 and 200 columns.
4. Band A is exactly one row for every tool, including a 300-character command.
5. No body row soft-wraps at any width.
6. A `read` of 500 lines, a `shell` of 4000 lines and a 40-file `edit` each render in
   bounded rows with a working, addressable fold handle.
7. Resizing 120 → 80 → 120 leaves the transcript byte-identical apart from truncation.
8. No tool outcome, path, provider name or error text is rendered at FAINT.
9. The only inverted cells on screen are decision keys and the focused picker row.
10. `P1_REDUCED_MOTION=1` produces a screen with zero animation frames.

---

## 11. Non-goals

Unchanged from `SPEC.md` §8, plus:

- No per-tool colour coding, no tool icons, no badges.
- No collapsing of settled blocks into one-line summaries — expanded is the default.
- No rewriting a block after it is drawn, other than the running block's own header row.
- No nested blocks. A worker's output is rows inside band B, never a block inside a block.
