# p1 TUI — implementation handoff (SLAB Harness)

For the Rust implementer of `crates/p1-tui` (state machine + cell renderer, ratatui) and the
driver in `crates/p1-host/src/tui.rs` (ADR-0043). This document is the build contract. Where it
and `docs/design/tui/SPEC.md` disagree, this document wins; every such place is listed in §13.

Source of truth for the mocks: `lib/p1-cells.js` (layout engine) + `lib/p1-screens.js` (states) +
the component recipes in `components/`. Every grid below is generated from them, so the visual kit
(`p1 TUI Screens.dc.html`) and these grids are the same cells. `handoff/grids.json` holds every
mock in machine-readable form (text rows + attribute runs) for `TestBackend` snapshot tests.

Verified against `5omeOtherGuy/phaseone@main` (`5b9c49a`, 2026-09-22). The brief names branch
`task/12-tui`; it no longer exists — the TUI work (M1–M4c) is merged on `main`, so `main` was read.

## Contents
1. How to read a mock
2. Colour tokens, 256-colour and NO_COLOR
3. Vocabulary: surfaces, ink roles, glyphs
4. Geometry and the responsive decision table
5. The row rule (Band): padding, truncation, wrapping
6. Transcript elements
7. Tool Blocks: the generic recipe and per-tool rules
8. Composer, queue, scrolling, focus mode
9. Pane: one model, four modes
10. Statusline: fields, data, drop order
11. Event and state → element map
12. Keybindings
13. Departures from `docs/design/tui/SPEC.md`
14. Proposals (needed, not planned)
15. Open questions for the owner
16. Screens (full-size mocks)

---

## 1. How to read a mock

Every mock is two fenced blocks.

**TEXT** — the exact characters, one line per terminal row, at the mock's real width. Rows are
prefixed `NN ` (0-based row number, not part of the screen). A ruler above gives the 0-based
column (tens / units). Trailing spaces are significant and are kept.

**RUNS** — one line per row: `NN ` then run-length attribute runs `<bg><fg>×<n>` left to right,
covering exactly the row width.

| bg code | surface | | fg code | role | | fg code | role |
|---|---|---|---|---|---|---|---|
| `G` | GROUND `--h-ground` | | `i` | ink | | `r` | ref (paths) |
| `B` | BLOCK `--h-block` | | `d` | dim | | `s` | syntax |
| `P` | BLOCK+ `--h-block-plus` | | `f` | faint | | `g` | ground (text on a fill) |
| `+` | diff-add bg | | `a` | attn (amber) | | `u` | rule (unfilled bar) |
| `-` | diff-del bg | | `x` | fail | | `+` / `-` | diff-add / diff-del fg |
| `A` | amber fill (decision key, focused row, cursor) | | `o` | ok | | `_` | blank cell (no glyph; fg irrelevant) |
| `N` | ink fill (route chip, neutral inversion) | | `l` | live (cyan) | | | |

Example: `Pa×2 Pi×38` = two cells amber-on-BLOCK+, then 38 cells ink-on-BLOCK+. `▪` cells coded
`l` are the only animated cells (§3.4). The composer cursor is drawn as one `A` cell in mocks;
at runtime it is the terminal's hardware cursor placed on that cell (see §8.1).

Component-level mocks (§6–§10) are drawn at their column width only (76 = the transcript column at
both 80 and 120 columns — see §4 — and 56 = the transcript at 100 columns). Place them at column 2.

---

## 2. Colour tokens

24-bit is the design target. Detect: `COLORTERM=truecolor|24bit` → 24-bit; `TERM=*-256color` →
256; `NO_COLOR` set (any value) → no colour (wins over everything); otherwise 256.

| Token | Role | 24-bit | 256 | NO_COLOR |
|---|---|---|---|---|
| `--h-ground` | GROUND surface | `#0a0a0a` | 232 | terminal default bg |
| `--h-block` | BLOCK surface | `#121212` | 233 | default bg |
| `--h-block-plus` | BLOCK+ surface | `#1c1c1c` | 234 | default bg |
| `--h-rule` | unfilled bar cells | `#2a2a2a` | 235 | bar cell drawn as ` ` (space) |
| `--h-ink` | values, arguments | `#e8e8e8` | 254 | default fg |
| `--h-dim` | tool names, labels, body | `#9a9a9a` | 247 | default fg |
| `--h-faint` | hints, line numbers, fold meta, queued | `#6a6a6a` | 242 | SGR 2 (faint) |
| `--slab-attn` | `!`, `›`, decision keys, focus row, cursor | `#e2a03f` | 179 | SGR 7 (reverse) on fills; glyph plain |
| `--slab-fail` | `✗`, `−N` | `#e0705f` | 167 | plain |
| `--slab-ok` | `✓`, `+N` | `#8fb573` | 107 | plain |
| `--slab-live` | running `▸`, `▪▪▪` | `#72b8b0` | 73 | plain; `▪▪▪` static |
| `--slab-ref` | paths, `file:line` | `#7fa7d6` | 110 | plain |
| `--slab-syntax` | code keywords (reserved; unused in p1 today) | `#c08fc8` | 176 | plain |
| `--slab-diff-add-bg` / `-fg` | added diff rows | `#3a4a3a` / `#d8e8d0` | 22 / 194 | plain; `+` column carries it |
| `--slab-diff-del-bg` / `-fg` | removed diff rows | `#4a3535` / `#e8d0d0` | 52 / 224 | plain; `−` column carries it |
| `--h-decision-key-bg` = `--h-focus-row-bg` | amber fill | `#e2a03f` | 179 | SGR 7 |
| `--h-route-chip-bg` | statusline chip fill | `#e8e8e8` | 254 | SGR 7 |
| text on any fill (`g`) | | `#0a0a0a` | 232 | (reverse video) |

256-colour picks are the nearest xterm-256 entries by RGB distance; the two diff fills trade
their muted tone for a saturated 22/52 because the grey ramp (236–238) would lose the hue.

**NO_COLOR must still read** — guaranteed by construction, and snapshot-tested with colour
stripped (the existing test rule "every state reads with colour stripped" stays):
- Every state is carried by a glyph in a fixed column (`▸ ✓ ✗ ! · ▪ ↳ ›`) and by fixed words
  (`denied`, `cancelled`, `exit 101`, `1 of 3 files`).
- Surfaces vanish; the one blank row between events (§6.1) and the 2-cell band padding keep the
  rhythm. Nothing relies on a surface to be understood.
- Fills become reverse video: decision keys, the focused menu row, the route chip, the cursor.
- The context bar keeps filled `█` cells and draws unfilled cells as spaces; the percentage next to
  it is the value.
- `P1_REDUCED_MOTION=1` and NO_COLOR both freeze `▪▪▪` to three static cells.

---

## 3. Vocabulary

### 3.1 Surfaces (exactly three)
GROUND: transcript ground, operator input, prose, meta rows, queue rows, gutters, insets.
BLOCK: tool body and meta bands, pane, composer hint row, menu rows, scroll mark.
BLOCK+: tool header band, decision row, composer input row, statusline, peek, menu header.

### 3.2 Ink roles
INK = values and content (arguments, prose, numbers). DIM = labels, tool names, body text,
outcome facts. FAINT = only: key hints, line numbers, fold meta, queued items, unavailable rows,
timestamps. Never put something the operator must read in FAINT.

### 3.3 Glyphs (fixed hue each)
| Glyph | Meaning | Hue |
|---|---|---|
| `›` | operator input, operator-invoked command, composer prompt | attn |
| `▸` | tool call (settled: dim; running: live) | dim / live |
| `✓` | completed / ok outcome | ok |
| `✗` | failed, denied, blocked, stalled outcome | fail |
| `!` | blocked on the operator (approval, needs review) | attn |
| `·` | meta, queued, folded, cancelled, unknown | faint |
| `↳` | worker / nested activity | dim |
| `▪` | working (only as `▪▪▪`) | live |

Plus one quantity glyph: `█` (U+2588, block element, not box drawing) for the context bar only.
Key-hint symbols (`⏎ ⌥ ⇧ ← → ↑ ↓`) appear only inside FAINT key hints.

### 3.4 Working indicator
`▪▪▪`, three cells, live hue. Opacity per cell `0.18 → 1.0` raised-cosine, 1.1 s cycle,
0.18 s stagger (`glyphs::working_opacity`, unchanged). Implemented as fg scaled toward the
cell's bg. `P1_REDUCED_MOTION=1` → three static cells at full live. It is the only animation.

### 3.5 Copy rules
Facts joined with ` · `. Lowercase tool names exactly as the model calls them. Unknown quantity
`—` (never `0`, never blank). Fold handle `[h-xxxxxxxx]` (32-bit hex, as `fold.rs`). Durations
from `render::elapsed` (`412ms`, `11.4s`, `2m10s`). Token counts from `render::tokens`
(`846`, `12.4k`, `120k`). Cost `render::cost_string` (`$0.0123`). Key hints in caret notation.
No banners, no apologies, no role labels, no emoji.

---

## 4. Geometry

### 4.1 Columns
```
W < 100 :  inset 2 | transcript T = min(W−4, 120) | inset rest
W ≥ 100 :  inset 2 | transcript T | gutter 2 | pane P | inset 2        T + P = W − 6
```
Pane width `P` by `^W` state (cycle `narrow → wide → split → off → narrow`; the start state is
`narrow` below 160 columns and `wide` at 160+):

| ^W state | P | skipped when |
|---|---|---|
| narrow | 38 | W < 100 |
| wide | 56 | W − 62 < 56 (W < 118) |
| split | ⌊(W − 6)/2⌋ | ⌊(W − 6)/2⌋ < 56 (W < 118) |
| off | 0 | never |

Transcript cap: `T ≤ 120`. When `W − 6 − P > 120` the surplus goes to the pane (`P = W − 126`).
With the pane off and `W − 4 > 120`, the transcript is 120 and the rest is GROUND on the right.
Inside the transcript every band has 2 cells of padding, so the text measure is `U = T − 4` and
text starts at screen column 4. Pane bands have 4 cells of padding: pane grid `G = P − 8`.

| Terminal | T | U | P | pane grid | text col | pane col |
|---|---|---|---|---|---|---|
| 80×24 | 76 | 72 | — (overlay 38 via ^L) | 30 | 4 | 40 (overlay) |
| 100×30 | 56 | 52 | 38 | 30 | 4 | 60 |
| 120×40 (reference) | 76 | 72 | 38 | 30 | 4 | 80 |
| 120×40, ^W wide | 58 | 54 | 56 | 48 | 4 | 62 |
| 160×48 | 98 | 94 | 56 | 48 | 4 | 102 |
| 200×50 | 120 | 116 | 74 | 66 | 4 | 124 |
| 240×60 | 120 | 116 | 114 | 106 | 4 | 124 |

Note the useful accident: at **80 and 120 columns the transcript is the same 76 cells**, so every
transcript element renders identically at the floor and at the reference. Only the pane and the
statusline differ.

### 4.2 Rows
| H | top blank | transcript rows | gap | composer | gap | statusline | bottom blank |
|---|---|---|---|---|---|---|---|
| ≥ 30 | 1 | H − 7 | 1 | 2 (input + hints) | 1 | 1 | 1 |
| 20–29 | 0 | H − 4 | 1 | 2 | 0 | 1 | 0 |
| 13–19 | 0 | H − 3 | 0 | 2 | 0 | 1 | 0 |
| ≤ 12 | 0 | H − 1 (composer hidden) or H − 2 | 0 | 0 or 1 (input only) | 0 | 1 | 0 |

At ≤ 12 rows focus mode is on automatically (§8.5). The pane (when shown) runs from the first
row to the composer's last row; its first row is padding, its last row is the mode strip (§9.1).
At 120×40: transcript rows 1–33, gap 34, composer 35–36, gap 37, statusline 38, blank 39;
pane rows 1–36. At 80×24: transcript 0–19, gap 20, composer 21–22, statusline 23.

Resizing is live: on `Event::Resize` recompute geometry, re-wrap every block at the new `U`
(wrapping is a pure function of text and width), keep `scroll_top` anchored to the same
absolute transcript row, and never change a settled row's content — only its wrapping.

### 4.3 Responsive decision table
| Condition | Pane | Transcript | Composer | Statusline | Other |
|---|---|---|---|---|---|
| W ≥ 160 | wide 56 by default | W − 62 (cap 120) | full | full, all fields | — |
| 118 ≤ W < 160 | narrow 38 by default; ^W → 56/split allowed | W − 44 | full | full | — |
| 100 ≤ W < 118 | narrow 38 only | W − 44 (56–73) | full | drop order step 1–2 | WORKERS/OUTPUT use the compact 30-grid form |
| W < 100 | off; `^L` = overlay | W − 4 | full | drop order to fit | `▪ N workers` appears in the statusline |
| H ≥ 30 | rows 1..H−4 | H − 7 rows | 2 rows | yes | 1 blank row top and bottom |
| 20 ≤ H < 30 | rows 0..H−3 | H − 4 | 2 | yes | no blank rows |
| 13 ≤ H < 20 | rows 0..H−3 | H − 3 | 2, no gap | yes | fold bodies keep 4 rows, not 8 |
| H ≤ 12 | hidden (focus) | H − 1 / H − 2 | hidden while empty, 1 row when shown | yes | approvals still take the screen |
| blocking diff review | hidden | full width W − 4 | hidden | yes | §7.5 |

---

## 5. The row rule (Band)

Every row is a `Band`: `{bg, left[], right[], width, pad}`; `left`/`right` are `[tone, text]`
segments. This is exactly SLAB Harness `Band` and must be ported 1:1:

```
U    = width − 2·pad
over = len(left) + len(right) + (right non-empty ? 2 : 0) − U
for seg in left, from the LAST to the first, while over > 0:
    if len(seg) > over + 1:  seg = seg[..len(seg) − over − 1] + "…" ; over = 0
    else:                    over −= len(seg); seg = ""
gap  = max(0, U − len(left) − len(right))
row  = pad spaces · left · gap spaces · right · pad spaces      (clipped to width)
```
- **Right never truncates; left truncates from its end with one `…` cell.** So the outcome of a
  tool always survives and the argument loses its tail.
- Exception, **path targets truncate in the middle of the path** keeping the file name:
  `crates/p1-con…/src/edge.rs`. The target segment is pre-cut before the Band rule runs:
  keep the last path component whole, cut the front at a `/` where possible, put `…` where the cut
  is. If the file name alone does not fit, fall back to the Band rule.
- `len` is terminal cells: use `unicode-width` (the existing `wrap::cell_width`). Every glyph in
  §3.3 is width 1. A wide (CJK) char that would straddle the cut is replaced by `…`.
- A segment is never split across rows by the Band; wrapping happens before (below).

**Wrapping** (prose, operator input, meta rows, notices, reasoning): word-wrap at `U`, break a
word longer than a line hard at the cell, continuation rows indented by the element's hang
indent (operator input 2, meta 2, notice facts 12). `wrap.rs` already does this; keep it.

**Blank rows:** exactly one GROUND row between events; zero between bands of one event.

---

## 6. Transcript elements

### 6.1 Order and rhythm
The transcript is append-only. An event = one element below; one blank row separates events.
Only the **running** element (the last element when a turn is live) may change; everything
above it is settled and immutable except for re-wrapping on resize.

### 6.2 Operator input — `OperatorTurn`
`› ` attn at text col, then the prompt in ink, wrapped at `U − 2`, hang indent 2. A steering
message delivered mid-turn renders the same, with a right-aligned faint tag `steering`
(follow-ups become ordinary turns and carry no tag).
<!-- MOCK:el-operator -->

### 6.3 Assistant prose — `ProseFlow`
Ink, wrapped at `U`, no label, no bg. Paths the model writes in backticks render ref (the
backticks are dropped only when the content is a path that exists in the workspace — otherwise
the text is shown verbatim). No other markdown is interpreted.

### 6.4 Reasoning — `Reasoning`
Collapsed (default): `· ` faint + `reasoning 4.2s` dim; right `^R expand` faint. While reasoning
streams, the elapsed is live (`· reasoning 2.1s` updating, running element). `^R` toggles the most
recent block (existing `toggle_reasoning`); expanded body is dim, indented 2, wrapped at `U − 2`,
no bg (reasoning is conversation, not tool output); the hint becomes `^R collapse`.
Elapsed is measured from the first `ReasoningDelta` to the first non-reasoning event.
<!-- MOCK:el-reasoning -->

### 6.5 Turn working row — `TurnWorking`
Present while a turn is live **and no tool Block is running** (a running Block carries its own
`▪▪▪`). `▪▪▪` at text col, then dim `phase · elapsed`, right dim `request N`.
Phases: `waiting` (TurnStarted/RequestStarted until the first delta), `reasoning`, `streaming`,
`preparing <name>` (ToolInputDelta), `summarizing context`, `retrying` (proposal §14.2).
It is the last transcript row and is removed at `TurnFinished`.
<!-- MOCK:el-working -->

### 6.6 Meta row — `MetaRow`
`· ` faint + dim text, wrapped, hang indent 2; optional right facts dim. Used for
display-only facts: provider notices, context replacement, model switch, goal set, inbox
remainder, unknown slash command.
<!-- MOCK:el-meta -->

### 6.7 Turn notice — `TurnNotice` (turn endings and errors)
No banner and no background. Row 1: glyph (`✗` fail, `·` faint for cancel) + ink headline.
Then fact rows: 2 spaces + dim label padded to 10 + ink value (wrapped, hang 12). Labels in this
order, each only when known: `cost` (what the failed turn spent), `kept` (what is still true),
`next` (faint key/command hint). See §7.8 and the mocks there.

### 6.8 Worker report — `WorkerReport`
The host renders every worker's end itself (ADR-0050 §6) as a settled 3-band element on BLOCK:
row 1 glyph + id + route, right `elapsed · cost`; row 2 `grants` + tool names; row 3 `↳` + the
report line (`done`, `blocked: needs edit — tried edit ×2`, `failed: <kind>`, ...). See §7.7.

### 6.9 Command output — `CommandOutput`
Output of an operator slash command (`/help`, `/models`, `/status`, `/model` result lists) is a
settled element in the transcript, not an overlay: header BLOCK+ `› ` attn + command dim (10
field) + argument ink, right facts dim; body rows BLOCK. Operator-invoked things carry `›`;
agent tool calls carry `▸`.

### 6.10 Menus — `Menu` (completion, /model, /resume)
A menu is docked: it takes the bottom rows of the transcript area directly above the composer
(and the queue, if any), never floats, never covers the pane. Max 8 rows + `· N more`.
- Optional header BLOCK+: `› ` attn + command (dim, 10) + filter text (ink); right dim count.
- Group header rows BLOCK: dim uppercase label, right dim.
- Rows BLOCK: `· ` faint + label (ink, padded to the menu's label column) + description (dim);
  right dim. Unavailable rows: every segment faint, the reason as the right text; selection skips
  them (existing `move_selection`).
- Focused row: amber fill across the row, text ground, glyph `▸ `. Exactly one per menu.
- Footer BLOCK: left dim note, right faint keys.
Filtering is the existing substring rule on the label (`Picker::visible`).

### 6.11 Home prelude — `HomePrelude` and `Monogram`
See §16 H-screens. The dotted `p1` mark (`●`, `─`, `│`) is replaced by a **slab monogram**: the
same 5×9 node layout from `render/home.rs` (`GLYPH_P`, `GLYPH_ONE`), each lit node drawn as 2
cells of BLOCK+ **background** (spaces — no glyph at all), letters 1 node apart: 22 × 9 cells.
Drawn only in unused transcript rows when the free area is ≥ 30 × 14; centred; `phaseone` ink and
`We love pie` dim below it, one blank row apart. Smaller: no mark, no words. NO_COLOR: no mark.
<!-- MOCK:el-monogram -->

---

## 7. Tool Blocks

### 7.1 The generic recipe (any tool, present or future)
Issue #46: a tool describes its own call as `{name, target, target_kind, outcome}`; the TUI never
matches literal tool names. Until #46 lands, the adapter table in §7.3 lives in ONE function in
`p1-host` (`tool_face(name, input, result) -> Face`) — never in `p1-tui`.

```
band A  BLOCK+  [glyph ][name field][target ........…]      [status glyph][ outcome facts]
band B  BLOCK   body rows (tool-specific, bounded)                                   (0..n)
band C  BLOCK   meta: fold line / facts                                  [right hint] (0..1)
decision BLOCK+ only while awaiting approval (§7.5)
```
- Glyph col 4 (text col): `▸ ` dim settled, `▸ ` live while running, `▸ ` dim + name `…` faint
  while preparing (ToolInputDelta; the name is not in the event — proposal §14.1), `! ` attn
  while awaiting approval.
- Name field: 10 cells, dim, the tool's call name verbatim. **A name of 10+ cells is never cut**:
  it takes `len + 1` cells and pushes the target right for that row only (`apply_patch`,
  `worker_start`, `worker_continue` collide if cut — DS `Block` cuts at 9 + `…`; departure §13).
- Target: ink; paths ref and middle-truncated (§5); newlines in a target shown as `␤`, bounded at
  100 chars before the Band rule (existing `summarize_input`).
- Right, running: `elapsed` dim (live, whole tenths) + 2 spaces + `▪▪▪`. Right, preparing: input
  size dim (`1.4 kB`). Right, settled: status glyph + space + outcome facts dim:

| ToolStatus | glyph | outcome prefix |
|---|---|---|
| Ok | `✓` ok | tool facts |
| Error | `✗` fail | tool facts, else first output line (≤ 40 cells + `…`) |
| Denied | `✗` fail | `denied` (+ ` · reason` when the operator gave none it is `denied by operator`) |
| Cancelled | `·` faint | `cancelled` |
| Unknown (reconciled at turn end) | `·` faint | `unknown · turn ended` |
| Unavailable | `✗` fail | `unavailable · not assembled` |

**Body rule:** ok → no body unless the per-tool row says otherwise. Not ok → the output as body,
dim on BLOCK, indent 2 (text col + 2). Output ≤ 12 lines: all of it. Over 12 lines: keep 8
(**head** for every tool, **tail** for `shell` — errors are at the end), then band C
`· N more lines folded → [h-xxxxxxxx]` (shell: `· N earlier lines folded → [h-…]`) faint,
right `^O open in pane` faint. At H < 20 keep 4 instead of 8. Body lines longer than `U − 2`
cut with `…` (never wrapped — output is evidence, wrapping lies about line structure).
Every output over 12 lines is registered under its handle whether ok or not; `^O` opens the
most recent handle (existing `latest_fold`).

**Diff body rows** (edit, apply_patch, write under review): `NNN` faint line number right-
aligned to the widest number in the block (min 3), 2 spaces, sign (`+`, `−` U+2212, or space)
and a space, then the code line; bg diff-add/diff-del for changed rows (fg diff-add/del),
BLOCK for context rows (fg dim). The fill spans the whole band width.

### 7.2 Mocks: generic, running, preparing, statuses
<!-- MOCK:el-tool-states -->
<!-- MOCK:el-tool-states-56 -->

### 7.3 Per-tool rules
| tool | target | ok outcome | ok body | not-ok body / notes |
|---|---|---|---|---|
| `read` | `file_path` (ref), `:a-b` when offset/limit | `N lines · S kB` | none | error text (`no such file`, confinement refusal) |
| `write` | `file_path` (ref) | `N lines · S kB · new` or `· replaced` | none | error; under `--ask` review is a diff of all-added (new) or old→new |
| `edit` | `file_path` (ref) | `+A −R` | the hunk: up to 8 diff rows (context 1 before/after), then band C `· N more diff rows → [h-…]` | error (`old_string not found`) — no diff |
| `apply_patch` | `N files` when > 1, else the one path (ref) | `+A −R · N files` | one row per file: path (ref) left, `+A −R` dim right; max 8 + `· N more files` | patch rejection text; multi-file review §7.5 |
| `grep` | `pattern` ink + ` ` + path scope ref | `N hits · F files` (`0 hits` is ok) | none | error text |
| `shell` | `command` ink | `D · exit C · N lines` | none | tail rule; band C carries `cwd … · bubblewrap` facts dim when the shell runs sandboxed, and `^O open in pane`. Exit code from the tool's result header |
| `finish` | `done` / `blocked` | done: `verified · <cmds>`; worker without a command tool: `done · not verified — parent verification required` | done: one row per verification command `✓ cmd` | rejected (tool error): `✗ rejected · no successful run of "<cmd>" after the last change`; blocked: `✗ blocked · needs <x>` + summary body |
| `worker_start` | `w1 · env/profile` | `started · <grants>` | row 1: task first line (ink), row 2 `grants  read edit finish` | failed start (`apply_patch` not expressible on route): error text |
| `worker_continue` | `w1` (+ ` +tool …` when `add_tools`) | `resumed` (`· +edit`) | none | error |
| `worker_result` | `w1` | `<finish status> · N lines` | structured report lines (tools, finish, needs, missing calls), max 8 | error |
| `worker_cancel` | `w1` | `cancelled` | none | error |
| any other | `Face.target` or the salient field or bounded raw input | `Face.outcome` or `N lines` | none | generic head rule |

Facts that are not known are omitted, never guessed (`S kB` needs the result size; `N hits`
needs a count the tool reports — until #46 the host counts result lines for `grep`).
<!-- MOCK:el-tools -->
<!-- MOCK:el-tools-fail -->
<!-- MOCK:el-workers-tools -->

### 7.4 Fold handles
Unchanged from `fold.rs`: 32-bit content hash, stable across resume, `h-` + 8 hex. The only
changes: the inline threshold is 12 (not 40) and the kept rows are 8 (4 at H < 20); shell keeps
its tail. SLAB tokens `--h-fold-threshold: 40` / `--h-fold-keep: 24` are superseded (§13).

### 7.5 Approvals (only under `--ask`, ADR-0038)
Two forms, one decision model. The decision is always about the **whole call** (all files of an
`apply_patch`).

**Inline (default)** — the call's Block in the transcript becomes the running element with `!`
attn, the body shows the facts, and a Decision band closes it:
- permission (shell and any non-diff tool): body = label/value rows `cwd`, `sandbox`, `network`,
  `effect` (dim label padded 10, ink value); `from w1 · env/profile` first when a worker asked.
- diff (edit, write, apply_patch): body = the diff when it fits in `transcript rows − 8`; else the
  first 8 rows + band C `· N more rows · ^D full review`.
- Decision band BLOCK+: ` y ` amber-inverted + ` allow once` ink, 3 spaces, ` a ` + ` session`,
  ` p ` + ` project`, ` n ` + ` deny`. Right faint: `^D review` (diffs) and `1 of 2 pending` when
  more requests are parked (existing `pending_auth` queue).
- Ungrantable keys stay visible, faint, with the reason on their own BLOCK row under the decision
  band: ` a   session    not grantable — destructive floor`. `p` is shown faint with
  `no trust store yet` until project grants exist (today `p` silently equals `a`; §13).
- One amber event per view: while a decision is on screen, the composer `›` turns dim, menus
  cannot open, and no peek appears. A second request waits (counted, not shown).

**Full review (`^D`, or automatically when the diff is taller than the transcript area)** — the
blocking view owns the screen from row `top` to the statusline gap, full width `W − 4`; pane and
composer hidden: header BLOCK+ `! ` + tool + path (ref), right `1 of 3 files` ink; summary row
BLOCK dim; the diff body scrolls (`PgUp PgDn`); decision band pinned to the bottom (never scrolled
away), hint row BLOCK faint `tab next file   ⇧tab previous   ^D back`. `tab` pages files (view
only); `^A all files` is removed — `y` already allows every file of the call.
After the decision the Block settles: `✓ allowed once` / `✓ allowed · session grant` then runs, or
`✗ denied` and stays in the transcript (nothing disappears).
<!-- MOCK:el-approval-permission -->
<!-- MOCK:el-approval-floor -->
<!-- MOCK:el-approval-edit -->

### 7.6 Streaming a tool's arguments (ToolInputDelta)
From the first `ToolInputDelta{call_id}` a preparing Block is the running element: `▸` dim, name
`…` faint (unknown until ToolStarted), target = the last line of the accumulated input (dim,
tail-truncated from the LEFT so the newest text shows), right = accumulated size. For inputs over
one line (write content, apply_patch) band B shows the last 3 lines dim. At `ToolStarted` with the
same `call_id` the Block becomes the normal running Block (name and target from the call).
Input deltas are display-only: nothing is kept after `ToolStarted`.

### 7.7 Workers in the parent transcript
- `worker_start` Block (settled at once) — the parent's view of the start.
- The live state never re-renders the transcript: it is in the WORKERS pane (§9.4), and without a
  pane in the statusline (`▪ 2 workers`).
- The worker's end → `WorkerReport` (host note, ADR-0050 §6), whatever the parent says later.
- The parent's own `worker_result` / `worker_continue` / `worker_cancel` Blocks.
- The current driver note `↳ w1 started` is dropped (the worker_start Block says it); the
  `↳ w1 finished` note is replaced by the WorkerReport.
<!-- MOCK:el-worker-report -->

### 7.8 Turn endings and errors
| TurnEnd / cause | headline | cost | kept | next |
|---|---|---|---|---|
| Completed{EndTurn/ToolUse} | — (nothing rendered) | | | |
| Completed{MaxOutputTokens} | `· stopped · max output tokens` | spend of the turn | | |
| Completed{ContextWindowExceeded} | `✗ context window exceeded · <used> of <window>` | | journal | `/model` bigger window |
| Completed{Refusal} | `· stopped · the model refused` | | | |
| Completed{Paused / Other} | `· stopped · paused by the provider` / `· stopped` | | | |
| Cancelled (^C) | `· cancelled at 12.4s` | requests + in/out of the turn | settled calls; the running call settled `cancelled`; `dropped 1 queued` | |
| ProviderFailed Authentication | `✗ authentication failed · <route>` | | journal | `p1 login <route>` |
| ProviderFailed InsufficientBalance | `✗ account exhausted · <route> · not retried` | | journal | `/model` another route |
| ProviderFailed NotEntitled | `✗ not included in the plan · <message> · not retried` | | journal | `/model` another route |
| ProviderFailed UsageLimitExhausted | `✗ usage limit reached · <message> · not retried` | | journal | wait for the reset, or `/model` |
| ProviderFailed RateLimited | `✗ rate limited · <route>` | | journal | `wait, or /model` |
| ProviderFailed Transport | `✗ connection failed · <message>` | lost streamed text size | journal | `⏎ resend` |
| ProviderFailed Protocol / InvalidRequest | `✗ provider error · <kind> · <message>` | | journal | |
| CommitFailed | `✗ journal commit failed · <message>` | | `nothing after the last committed record happened` | `restart p1 to resume from the journal` |
| ContextFailed | `✗ context failed · <message>` (e.g. `at the wall: 118k of 120k`) | the summarization request's usage or `—` | history unchanged | `/model` bigger window |
| stalled (headless guard; interactive: proposal §14.7) | `✗ stalled · 6 summaries without a workspace change` | | | |

`cost` = sum of the turn's `ResponseCompleted` usages and `ContextReplaced` usages
(`in X · out Y · $C` or `—`). `kept` always names the journal first (it is the single truth,
ADR-0021) then what the workspace holds and what still runs (`w1 running`).
The message text is `ProviderError.message` (already sanitised — never a body or credential).
<!-- MOCK:el-endings -->

---

## 8. Composer, queue, scrolling, focus

### 8.1 Composer
Row 1 BLOCK+: 2 pad, `› ` attn, text ink (placeholder faint), 2 pad. Row 2 BLOCK: hints faint
left, secondary faint right. Multiline text grows row 1 upward (existing wrap + `⌥⏎`), capped at a
third of the screen (existing cap). The hardware cursor sits on the text cell; request an amber
block cursor with `OSC 12;#e2a03f` + `DECSCUSR 2` where supported, and restore on exit.

| state | placeholder / text | hints (left) | secondary (right) |
|---|---|---|---|
| idle | `message, / for commands` | `⏎ send   ⌥⏎ newline` | `^C quit` |
| working | `steer the running turn` | `⏎ queue steering   ⌥⏎ queue follow-up` | `^C cancel` |
| `/` typed | `/mo` | `tab complete   ⏎ run` | `esc dismiss` |
| goal edit (`^G`) | `/goal fix compaction boundary stall` | `⏎ set goal   empty ⏎ clears` | `esc keep` |
| approval on screen | `› ` dim, `decide above` faint | (empty) | `^C cancel turn` |
| attached to a worker | `› ` dim, `attached to w2 — read only` faint | `esc detach   x stop` | `PgUp scroll` |
| H 13–19 | row 2 kept | | |
| H ≤ 12 | row 1 only, hidden while empty | | |

### 8.2 Queue
Queued input sits on GROUND directly above the composer gap, oldest first, FAINT:
`· steering   <text>` right `next boundary`; `· follow-up  <text>` right `after this turn`.
Text is cut with `…` at the row. On `InboxDelivered{count}` the oldest `count` steering rows leave
the queue and appear in the transcript as `OperatorTurn` tagged `steering` at that point; if
`count` exceeds the queued steering rows, the remainder is a meta row `· 1 inbox message
delivered` (worker completions arrive through the same inbox). A follow-up fires when the agent
would stop and becomes an ordinary operator turn. On cancel the queue is dropped and the notice
says `dropped N queued`.

### 8.3 Scrolling
`PgUp`/`PgDn` scroll by `transcript rows − 2` (was 10). Scrolled back, the view is pinned to its
absolute top row (existing `scroll_top`); new output never moves it. The last transcript row
becomes a **scroll mark** (BLOCK): `· ` faint + `N` ink + ` new rows below` dim, then — if a turn
is live — ` · ▸ shell running` (glyph live); right faint `row A of B   esc live tail`. `esc` (when
no overlay is open) or `PgDn` past the end returns to the live tail and removes the mark.
<!-- MOCK:el-queue-scroll -->

### 8.4 Goal editor
`^G` (or `/goal` with no argument) puts `/goal <current goal>` in the composer with the cursor at
the end (existing `EditGoal`, now prefilled). `⏎` sets it (meta `· goal set` and LEDGER GOAL
updates); an empty goal clears it (`· goal cleared`); `esc` restores the previous composer text.
The goal stays host-owned, shown as a quotation in LEDGER, never a tool.

### 8.5 Focus mode (SPEC §4.3a restated)
Pane hidden. Composer hidden **while empty**; the first key, paste or `^G` reveals it; submit or
clearing hides it again. Statusline stays (it is the only persistent context). Approvals and full
review always take the screen. `/focus` toggles, `/focus on|off`; `off` returns to the automatic
policy (on at ≤ 12 rows). Transcript grammar is unchanged — only visibility changes.

---

## 9. Pane

### 9.1 One pane, four modes
The DS pane sections (SESSION / LEDGER / WORKERS / FOLDS) and the old SPEC modes (LEDGER /
OUTPUT / DIFF / WORKERS) reconcile into **modes** that each own the whole pane:

| mode | contents | available when |
|---|---|---|
| LEDGER | GOAL, SESSION, CONTEXT, WORKSPACE, SPEND, WORKERS (1-line summary), FOLDS (last 3 handles) | always |
| OUTPUT | a fold handle's full output, scrollable | a handle was opened (`^O`) |
| WORKERS | one block per worker | a worker was ever started this session |
| DIFF | session diff by file | the diff seam exists (planned, §14.4) |

`^Tab` cycles only through available modes (fallback `^N`, §12). The pane's **last row** is the
mode strip: available modes left, the current one dim, the others faint; right faint `^Tab` (or
`pinned ^P`). The pane's first row is padding. TASK becomes **WORKSPACE** (p1 has no task ids):
files changed, `+/−` (when tracked), journal recency. GOAL stays the first LEDGER section; without
a pane it is visible in `/status`.

Promotion (existing rules, unchanged unless noted): a live worker promotes WORKERS while any worker
runs; a worker needing review self-pins WORKERS; `^O` opens OUTPUT; a failed tool or context ≥ the
summarize threshold shows a **peek** (2 BLOCK+ rows over the pane top, 3 s, never over a pinned or
blocked state). Approvals no longer promote a DIFF pane (they are inline or full review, §7.5).

### 9.2 LEDGER
Grid = P − 8 (30 at P 38). Section header dim uppercase; rows: label dim at 2 indent, value ink
right-aligned (grid rule). A third column (count) right-aligns to the inner stop `grid − 12`.
Sections dropped whole when the pane is too short, lowest priority first:
FOLDS → WORKSPACE → context parts → SESSION → WORKERS summary → SPEND → CONTEXT → GOAL.

- **SESSION**: `model env/profile`, `effort`, `access full|ask`, `sandbox off|bubblewrap`.
- **CONTEXT**: header right `used / window` (`12.4k / 120k`), bar row: `█` ink × `round(f·n)` +
  `█` rule × rest (`n = grid − 7`, 23 at grid 30: the percent needs 5 cells and the Band's 2-cell minimum gap), right the percent ink; when used ≥ the summarize threshold the
  percent is preceded by `! ` attn. Parts rows (`system`, `files N`, `tools N`, `recent`) only
  once the host's context-stats seam exists (today: absent, not `—`). Last row `summarize at` +
  threshold. `used` = input total of the last `ResponseCompleted.usage` (uncached + cache read +
  cache write); unknown → header `— / 120k`, bar empty, percent `—`.
- **WORKSPACE**: `files` (distinct paths changed by successful edit-shaped calls), `diff` (`+A −R`
  or `—` until tracked, §10), `journal` (`2m ago` since the last committed record).
- **SPEND**: `in`, `out`, `cache hit`, `cost` — the existing `Spend` rules (unknown poisons that
  part forever; the section is absent before the first response).
- **WORKERS**: `2 live · 1 done` + faint `^Tab`.
- **FOLDS**: up to 3 most recent handles: handle ref, right `shell · 94 lines` dim.
<!-- MOCK:el-ledger -->

### 9.3 OUTPUT
Header row: `OUTPUT` dim, right handle faint. Source row: `tool · target · ✗ exit 101` (glyph hue).
Range row right-aligned dim `12–41 of 94`. Body: line number faint (4) + 2 + text dim, cut `…`.
`^F` focuses the pane; then `↑ ↓ PgUp PgDn` scroll, `esc` returns focus. (Today `↑ ↓` scroll the
pane whenever OUTPUT is open — that steals composer keys; §13.)
<!-- MOCK:el-output -->

### 9.4 WORKERS
Header: `WORKERS` dim, right `2 live · 1 queued · pool 3/4`. Order: needs review, running, failed
and stalled, queued, done, cancelled, lost. Wide grid (48) — 2–4 rows per worker:
```
<glyph> <id>  <task first line, ink>                       <state, dim>
  <model ink> · <route dim, only when the whole suffix fits> <tok>/<ctx> · <elapsed ink> · <cost ink>
  grants  <tools, ink>                         (only when grants is non-empty)
  ↳ <current activity or end line>              (only when activity is non-empty)
```
The route is a dim suffix only when the whole suffix fits; a long model is cut with `…` and the route
is omitted. Compact grid (30) — 3 rows: glyph id task / state; model (with route suffix only if it
fits) / elapsed; `tokens` tok/ctx on the left and cost on the right. Unknown values render `—`.
States and glyphs: queued `·` faint, running `▪` live, needs review `!` attn (the worker has a parked
approval), done `✓` ok (and `done · not verified` when it finished without a command tool), failed
`✗` fail, cancelled `·` faint, stalled `✗` fail (`6 summaries without a change`), lost `·` faint
(`not restored on resume`, ADR-0034). Selection: `^F` focuses the pane, `↑ ↓` move an amber focus
row (row 1 of a block), `a` attach, `x` stop (asks `y stop  n keep` — the one amber event), `esc`
back. Rows changed by owner request (#111, 2026-09-24); the WORKERS mocks were re-derived from the
renderer because the WorkersPane mock component is not in the repo; the state in lib/p1-screens.js carries the new fields.
<!-- MOCK:el-workers-pane -->

### 9.5 Attached worker
`a` swaps the transcript area to the worker's own transcript (the child event stream, buffered
per worker from `child_event_sink`). Row 0 of the transcript area is an attach band BLOCK+:
`↳ ` dim + `attached w2` ink + ` · env/profile · running` dim, right faint `esc detach   x stop`.
The composer is read-only (§8.1), the statusline chip shows the worker's model. The parent keeps
running; a parent approval pulls the view back to the parent (safety wins).

### 9.6 Overlay (`^L`, W < 100)
The pane in its current mode drawn over the right of the transcript: `P = min(38, W − 4)`,
from the first row to the composer hint row, BLOCK, pad 4. `^L` or `esc` closes. Nothing
under it reflows.

---

## 10. Statusline

One BLOCK+ row at `x = 2`, width `W − 4`, pad 1.
Left: route chip (ink fill, ground text) ` env/profile `, `   repo` ink, ` branch` dim,
`   effort ` dim + level ink. Right: `▪ N workers` (live glyph, ink N) when any worker is live,
`   ctx ` dim + percent ink (`! ` attn before it at the warn threshold), `   spend ` dim + cost ink,
`   clock` ink, `   +A` ok ` −R` fail.

| field | source | unknown / absent |
|---|---|---|
| chip | `parent_assembled(route, model)` → the model reference `env/profile`; attached: the worker's | never absent |
| effort | `ModelOptions.reasoning_effort` (`low medium high max`, `xhigh`) | `default` (adapter default is known, not unknown) |
| repo | workspace root basename | never absent |
| branch | `git rev-parse --abbrev-ref HEAD` at start and at each TurnFinished; detached → 7-char sha | field omitted outside git |
| workers | worker snapshot: count of running | omitted at 0 |
| ctx | last usage input total ÷ configured context window (`[context]`) | `—` |
| spend | `Spend.cost_micro_usd` via `cost_string` | `—` (subscription routes: `reports_cost = false` → always `—`) |
| clock | wall time since the session started, `0h02` (`{h}h{mm}`) | never |
| +/− | **not tracked yet**: shows `diff —` (dim label, ink `—`) until a diff seam counts lines of every workspace change (edit, write, apply_patch AND shell). The driver's current edit-input counts are not shown: they miss apply_patch (#46) and shell writes | `diff —` |

Drop order when the row does not fit (`len(left) + len(right) + 2 > W − 6`), applied in steps
until it fits: 1 fold effort into the chip (`claude/opus-5.5:high`) · 2 drop clock · 3 drop branch
· 4 drop repo · 5 drop `diff` · 6 drop the `spend` label (value stays) · 7 the chip truncates by the
Band rule. Right never truncates.
<!-- MOCK:el-statusline -->

---

## 11. Event and state → element map

| Source | Element | Notes |
|---|---|---|
| `AgentEvent::TurnStarted` | `TurnWorking` phase `waiting` | clock for elapsed starts |
| `RequestStarted{request_index}` | `TurnWorking` right `request N` (1-based) | a retry shows the next index |
| `TextDelta` | `ProseFlow` (running, grows) ; `TurnWorking` → `streaming` | new block after reasoning (existing rule) |
| `ReasoningDelta` | `Reasoning` collapsed, live elapsed ; `TurnWorking` → `reasoning` | |
| `ToolInputDelta{call_id,text}` | preparing Block (§7.6) ; `TurnWorking` → `preparing` | display only |
| `ProviderNotice{text}` | `MetaRow` `· <text>` | never journalled; not replayed on resume |
| `ResponseCompleted{model,stop,usage}` | closes streams; `Spend.record`; LEDGER CONTEXT; statusline ctx/spend; stop ≠ EndTurn/ToolUse → §7.8 row | usage `None` poisons spend parts |
| `InboxDelivered{count}` | queued steering → `OperatorTurn` tag `steering`; remainder `MetaRow` | §8.2 |
| `ContextReplaced{before,after,usage}` | `MetaRow` `· context summarized · 214 → 31 items`, right `in 1.8k · out 0.6k` or `—`; LEDGER CONTEXT refreshes at the next response | after commit only (R6) |
| `ToolStarted{call}` | running Block (converts the preparing Block with the same call_id) | elapsed from the event stamp |
| `ToolFinished{result}` | settles the Block; fold registered; failed → peek | orphan result still renders |
| `TurnFinished{end}` | removes `TurnWorking`; running calls → `unknown · turn ended`; `TurnNotice` per §7.8 | |
| `TurnEnd::*` | §7.8 table | |
| `AuthRequest` (TuiPolicy) | inline approval Block or full review (§7.5); queued behind the one on screen | `--ask` only |
| approval answered | Block settles `✓ allowed …` / `✗ denied` | a cancelled turn drops parked requests (= deny) |
| `UiEvent::WorkerStarted(id)` | WORKERS pane row `queued`/`running`; promotion | no transcript row |
| worker child events | the worker's buffered transcript (attach); pane row 4 activity from the latest `ToolStarted` | |
| `FrontEnd::worker_ended` (ADR-0050 §6) | `WorkerReport` in the parent transcript; pane row state | |
| `ChildStatus::Running / Finished / Cancelled / Failed` | pane `running` / `done` (`done · not verified` from the finish report) / `cancelled` / `failed` | |
| worker queued in the pool | pane `queued` | needs a `Queued` status from the service (§14.3) |
| worker stall guard fired | pane `stalled` | |
| `announce_lost_workers` (resume) | pane `lost`; one `MetaRow` `· 2 workers not restored` | |
| worker with a parked approval | pane `needs review` (self-pins) | |
| `/model`, `/model REF`, `/effort LEVEL` | `switch_model` result → `MetaRow` `· model a → b · from the next turn`, or `TurnNotice`-style `✗ switch refused · <reason>` with `kept  still on a` | while a turn runs the switch applies at the next boundary |
| `/goal …`, `^G` | §8.4 | |
| `/focus` | §8.5 | |
| `/status`, `/help`, `/models` | `CommandOutput` | `/status` replaces today's overlay |
| `/resume` | `Menu` (session list) → on ⏎ the transcript is repainted from history (existing `paint_history`) + `MetaRow` `· resumed <session> · N items · env/profile` | a resume the provider refuses shows `✗ resume refused · <reason>` |
| `/access` | `CommandOutput` with the policy facts (`full`, `--ask` restarts to change) | the access mode is fixed per process (ADR-0038) |
| `/exit` | quit | |
| unknown `/x` | `MetaRow` `· /x is not a command · /help` | |
| scroll state | scroll mark | |
| terminal resize | re-layout (§4.2) | |

---

## 12. Keybindings

| Key | Context | Action | Notes / conflicts |
|---|---|---|---|
| `⏎` | composer, idle | send (slash command if `/…`) | |
| `⏎` | composer, turn live | queue steering | |
| `⏎` | menu open | run / accept the focused row | |
| `⌥⏎` | idle | newline | terminals must send ESC+CR; with the kitty keyboard protocol `⇧⏎` is also newline |
| `⌥⏎` | turn live | queue follow-up | a newline cannot be typed while a turn runs (unchanged; §15) |
| `^C` | turn live | cancel the turn (parked approvals are denied) | |
| `^C` | idle | quit | |
| `/` | empty composer | open command completion | |
| `tab` | completion | complete the focused command | |
| `↑ ↓` | menu | move focus (skips unavailable) | |
| `← →` | `/model` menu | change effort on the focused row | |
| `esc` | menu / overlay | dismiss | order: menu → overlay → pane focus → scroll → attach |
| `esc` | scrolled | jump to the live tail | |
| `esc` | attached | detach | |
| `y a p n` | decision on screen | allow once / session / project / deny | only while a decision is shown; otherwise they type |
| `^D` | decision with a diff | toggle full review | |
| `tab` `⇧tab` | full review | next / previous file | view only |
| `PgUp PgDn` | transcript / full review / attached | scroll | |
| `^O` | anywhere | open the latest fold in OUTPUT | |
| `^R` | anywhere | toggle the latest reasoning block | |
| `^Tab` | anywhere | next available pane mode | most terminals send plain `Tab` for `^Tab`; only kitty/xterm-modifyOtherKeys distinguish it → **`^N` is the always-working alias** |
| `^W` | W ≥ 100 | cycle pane width (narrow → wide → split → off) | readline's delete-word; the composer has no word delete, so no conflict today |
| `^L` | W < 100 | pane overlay | conventional "redraw"; p1 redraws on its own |
| `^P` | pane shown | pin / unpin the pane mode | |
| `^F` | pane shown | focus the pane (WORKERS select, OUTPUT scroll) | |
| `↑ ↓` | pane focused | select worker / scroll output | |
| `a` / `x` | WORKERS focused | attach / stop the selected worker | `x` asks `y stop  n keep` |
| `^G` | anywhere | edit the goal | |
| `^T` | — | removed (timeline was never wired) | |
| `^A` | — | removed (`y` allows all files) | |

---

## 13. Departures from `docs/design/tui/SPEC.md`

| # | SPEC | Now | Why |
|---|---|---|---|
| 1 | §1 monochrome; two diff hues only | SLAB/SIGNAL: five signal hues on glyphs and outcome markers | SLAB Harness is authoritative for p1's UI |
| 2 | §1 selection inverts ink/ground | focused menu row and decision keys invert on amber; route chip on ink | DS inversion rule |
| 3 | §2 `›` ink, `✗` ink, `!` ink, `▪` ink | fixed hues per §3.3 | DS glyph table |
| 4 | §3 "No bottom status bar" / §6 80-col floor line | a Statusline at every width replaces the floor line | DS Statusline; the facts it carries are needed at every width, not only at 80 |
| 5 | §3 tool rows are single lines with no bg | three-band Block (BLOCK+ header, BLOCK body/meta) | DS Block |
| 6 | §3 name field 12 (code: 10, cut at 9) | 10, never cut; long names take `len + 1` | worker tool names collide when cut |
| 7 | §4.3 fold at 40 lines, 8 head (DS tokens 40 / 24) | inline ≤ 12 lines, keep 8 (4 at H < 20); shell keeps the tail | a 24-row body cannot fit an 80×24 screen (20 transcript rows); shell errors are at the end |
| 8 | §4.4 diff review is always full screen | inline decision by default, full review on `^D` or when taller than the area | keeps context visible; DS Decision row |
| 9 | §4.4 `^D next file`, `^A all files` | `^D` toggles full review; `tab`/`⇧tab` page files; `^A` removed | one call = one decision; paging is view-only |
| 10 | §4.1 "no logo" / §9 dotted mark with `─ │` | slab monogram from BLOCK+ cells | `─ │` are box drawing; the owner still wanted a mark |
| 11 | §4.6 `/status` overlay | `CommandOutput` in the transcript | append-only, scrollable, one mechanism for all command output |
| 12 | §5 pane widths 40/56 | 38/56/split; T min 56, max 120 | DS geometry (2·76·2·38·2) |
| 13 | §5 DIFF promotion on approval | approvals are inline or full review; DIFF is the session diff (planned) | no second place to decide |
| 14 | §5 TASK section with task id | WORKSPACE section | p1 has no task ids |
| 15 | §5 LEDGER `warn at 60%` | `summarize at <threshold>`, warn when reached | the real config (`[context]` `summarize_at_tokens`, ADR-0036) |
| 16 | §5 WORKERS states `! ▪ ✓ ·` | + failed, cancelled, stalled, lost; `needs review` = parked approval | ADR-0034/0042/0050; no worktree-apply flow exists |
| 17 | §4.8 reviewed apply `r / y / n` | removed | workers write the shared workspace directly (ADR-0050) |
| 18 | §7 `^T` timeline | removed | never wired, not planned |
| 19 | code: `↑ ↓` scroll OUTPUT whenever open | only with pane focus (`^F`) | frees arrows for menus and future history recall |
| 20 | code: PgUp/PgDn 10 rows | screen − 2 | predictable paging at every height |
| 21 | §4.1 affordance `/env` | `/model` (the live command); `/env` becomes an alias | one command for the model |
| 22 | §2 `✗` for denied + INK marker | `✗` fail hue; cancelled/unknown use `·` | distinguish "didn't happen" from "failed" |

---

## 14. Proposals (needed, not planned)
1. **`ToolInputDelta` carries the tool name** (or a `ToolInputStarted{call_id, name}`), so a
   preparing Block can show its name. Contract change in `p1-contracts`.
2. **Interactive transient retry.** ADR-0041 retries only headless runs. Offer it in the TUI: on
   `Transport`/`RateLimited`/`Protocol` show `TurnWorking` phase `retrying · 1 of 3 · in 24s` and a
   meta row `· Transport: <msg> · retry 1 of 3`; `^C` stops the wait. Adapter back-off could reuse
   `ProviderNotice` (ADR-0048 names this).
3. **Worker service reports `Queued` and a usage tap** (per-worker cost, pool slot).
4. **A diff seam** (before-images at first mutation, including shell writes) → statusline `+/−`,
   WORKSPACE diff, DIFF pane.
5. **Session index for `/resume`**: id, started, first prompt or goal, env/profile, item count,
   owning pid (ADR-0031).
6. **`⏎ resend`** after a failed turn: resubmit the last operator text unchanged.
7. **Interactive stall warning**: after N summaries without a workspace change, a meta row
   `· 6 context summaries since the last change` (no cancel — the operator decides).
8. **Trust store** for `p` project grants; until then `p` is shown unavailable.
9. **Tool-provided destructiveness** so the destructive floor is not a UI pattern match.
10. **Context-stats seam** for the CONTEXT parts rows.
11. **Kitty keyboard protocol** opt-in (`^Tab`, `⇧⏎` distinguishable).

## 15. Open questions for the owner
1. Effort levels: the brief says `low medium high max`; `p1-contracts::Effort` also has
   `ExtraHigh`. Show `xhigh` where a profile offers it?
2. Keep `We love pie` under the monogram?
3. `/env` alias or remove it?
4. `finish blocked` and `worker blocked` use `✗` (didn't complete). Would you rather have `!`?
   (Recommendation: no — `!` means "your turn" and a blocked worker is the parent's turn.)
5. Show edit/apply_patch diffs inline on success (proposed) or collapse every ok call to one row
   (old SPEC)?
6. Should `^C` at idle need a second press to quit?
7. Not verified in the repo: the session id / file format for `/resume`, the exact exit-code line
   in shell results, and whether `grep` reports a hit count — the mocks assume them.

---

## 16. Screens
Full-size mocks. Each lists its geometry and the state it shows.

<!-- SCREENS -->
