# p1 TUI — design specification

Companion to `preview.html` (open in a browser for the rendered screens).
This file is the implementable form: exact values, no prose you have to interpret.

Target: 120x40 typical, **80x24 floor**. Dark ground only.

---

## 1. Palette

Monochrome. Two hues exist and are reserved for diff bodies only.

| Token | Hex | Use |
|---|---|---|
| GROUND | `#0a0a0a` | transcript background, terminal default bg |
| BLOCK | `#121212` | right pane, folded-output blocks |
| BLOCK+ | `#1c1c1c` | peek banner, inline command echo |
| RULE | `#2a2a2a` | unfilled bar segments |
| INK | `#e8e8e8` | primary text, **values**, active symbols |
| DIM | `#9a9a9a` | **labels**, secondary text, settled tool results |
| FAINT | `#6a6a6a` | key hints, line numbers, fold metadata, unavailable rows, pending items, timestamps |
| DIFF-ADD bg / fg | `#3a4a3a` / `#d8e8d0` | added diff lines |
| DIFF-DEL bg / fg | `#4a3535` / `#e8d0d0` | removed diff lines |

Selection and focus **invert**: `#e8e8e8` background, `#0a0a0a` text. That is the only
highlight mechanism in the interface.

### Contrast rule (non-negotiable)

FAINT is 3.4:1 and is legal ONLY for the six categories listed above. Anything the
operator needs to *read* is DIM (7.2:1) or INK. A tool outcome, a provider name, a file
path, a token count is never FAINT.

### Hierarchy rule

```
INK    = values and content
DIM    = labels
FAINT  = hints and unavailable states
```

Label/value rows are `DIM label` + `INK value`, e.g. `  sandbox  bubblewrap · writes: ...`.
Sibling rows in a list always share one value — never one provider at INK and its
neighbour at DIM.

---

## 2. Symbol vocabulary

Eight glyphs. Every state must be readable with all colour stripped; colour only
reinforces what the glyph already says.

| Glyph | Meaning | Colour |
|---|---|---|
| `›` | operator input | INK |
| `▸` | tool call | DIM |
| `✓` | completed | DIM |
| `✗` | failed | INK on the marker, DIM on the detail |
| `!` | approval required | INK |
| `·` | queued / folded / skipped | FAINT |
| `↳` | delegate / nested | DIM |
| `▪` | working | INK, LED chase |

**Working indicator:** three `▪` cells, 1.1s cycle, 0.18s stagger, opacity 0.18 → 1.0.
Not a braille spinner. Freezes to a static `▪▪▪` under `P1_REDUCED_MOTION=1`.

---

## 3. Chrome

- **No box-drawing characters anywhere.** Panes are separated by a one-step background
  shift (GROUND vs BLOCK) and by whitespace.
- No role labels, no bubbles, no left gutter bars on assistant prose.
- No bottom status bar, except the single 80-column fallback line (§7).
- Tool output earns chrome (background block, fold handle). Conversation does not.

### Column grid

Tool-call lines use a 12-column name field, then the argument, then a right-aligned
result:

```
▸ read      p1-context/src/edge.rs        ✓ 412 lines
▸ search    block_until_ready             ✓ 3 hits
▸ shell     cargo test -p p1-context boundary
  ✗ 11.4s · 12 passed, 1 failed · 94 lines      ^O open
! edit      p1-context/src/edge.rs          1 of 3 files
▸ delegate  2 workers                    ceiling 40 calls
```

Indentation is 2 spaces per level. Delegation indents **once and never more**.

---

## 4. Screens

### 4.1 Idle
Version line (`p1 0.1.0   ~/dev/phaseone   main`), one sentence of state
(`no journal in this directory.`), four affordances, prompt. No banner, no logo, no tips.

```
  /resume     reopen a previous session
  /env        claude · sonnet-4.5
  /access     full · --ask to confirm
  /goal       set the session objective
```

### 4.2 Streaming
Assistant prose unadorned. Reasoning collapses to `· reasoning 4.2s` with `^R` to expand.
Working indicator on its own line with a short label. Composer hints:
`⏎ queue steering   ⌥⏎ queue follow-up   ^C cancel`.

### 4.3 Tool call + fold
A settled call collapses to one line. Output over ~40 lines renders a bounded block on
BLOCK background with a fold handle `· N more lines folded → [h-7c21]`. The handle id is
stable and addressable — the right pane opens the same object (§6).

### 4.4 Diff review (blocking, full width)
Right pane hidden. Line numbers FAINT, context DIM, changed lines in the two hues with a
literal `+`/`-` column. Footer:

```
 y  allow once     a  session     p  project     n  deny
                            ^D next file   ^A all files
```

`y`/`a`/`p`/`n` are single-character decisions. Do not replace with a button row.

### 4.5 Permission prompt
Command echoed on a BLOCK+ line. Then label/value rows: `cwd`, `sandbox`, `network`,
`reason`. Destructive commands re-prompt every call and are **not grantable** — `a` and
`p` stay visible but FAINT with the reason inline:

```
 a  session      not grantable — destructive floor
 p  project      not grantable — destructive floor
```

### 4.6 Pickers
Max 8 rows, filter-as-you-type, one inverted selection row, `· N more` when truncated.
Group headers are DIM. Unavailable rows (`quota exhausted`, `not authed`) are FAINT.

Route picker groups by wire adapter (`ANTHROPIC ROUTE`, `OPENAI-CHAT ROUTE`), rows are
`route · profile`, then context window and per-Mtok in/out.

`/status` is a label/value overlay on a **40-column grid**, grouped with blank lines —
not comma-joined summary lines:

```
ROUTES
  claude                  oauth · cached
  deepseek                       api key
  glm                    quota exhausted

ENVIRONMENT
  route                           claude
  profile                     sonnet-4.5
  context                     configured

TOOLS
  assembled                            9
  shell                        sandboxed
  delegate                     route gpt

STORE
  journal                   ~/.config/p1

  esc
```

Unavailable rows (`quota exhausted`) are FAINT across the whole row, label included.

### 4.7 Goal & plan
Goal is a quotation, not an editable list — `/goal` is host-owned and never assembled as
a tool. Plan reuses the transcript symbols (`✓` / `▪` / `·`), no checkboxes. The single
working row is the only line at INK.

### 4.8 Delegation
Manifest line, one indented block per worker with route + profile, elapsed, calls, tokens.
Worker transcripts stay folded. A worker's mutations reach the parent only through a
reviewed apply (`r` review / `y` apply / `n` discard worktree).

### 4.9 Failure
No red banner, no apology, no retry spinner. State what broke, what it cost, what is still
intact, then hand back control. Denied calls stay in the transcript as denied results —
nothing disappears.

---

## 5. Right pane

Pane **width** cycles `off / 40ch / 56ch / split` with `^W`. Mode cycles with `^Tab`.
Background BLOCK (`#121212`). No border, no box-drawing — the background step is the edge.

| Mode | Pane width | Content grid | Contents |
|---|---|---|---|
| LEDGER | 40ch | 32 | goal, context budget breakdown, task diff, spend |
| OUTPUT | 56ch | 48 | the expanded form of a fold handle; scroll, filter, yank |
| DIFF | 56ch | 48 | non-blocking review; same keys and hues as §4.4 |
| WORKERS | 56ch | 48 | one row per delegate, ordered by what needs you |

**Pane width** is the column the pane occupies on screen. **Content grid** is the column
count its rows are laid out on, inside that width — the difference is the pane's padding.

### The grid rule

Every label/value row is **one label at the left edge of the grid and one value
right-aligned to the grid width**. No value is positioned by eye or by a counted run of
spaces; each row is `label + pad + value` computed against the grid width. A row with a
third column (a count) right-aligns that count to a fixed inner stop — 20 in the ledger.

This is what keeps a pane readable as a table rather than a paragraph of fragments, and
it is the single rule to re-apply whenever a string's length changes.

### LEDGER — 32-column grid

```
GOAL
fix compaction boundary stall

CONTEXT             12.4k / 200k
██████████████████████████    6%
  system                    1.2k
  files            4        6.8k
  tools           11        3.1k
  recent                    1.3k
  warn at 60%             120.0k

TASK                      t-3f9a
  files                        3
  diff                   +48 −12
  journal                 2m ago

SPEND
  in                       38.1k
  out                       4.2k
  cache hit                  71%
  cost                         —
```

Bars are `█` at INK over `█` at RULE — never `░` or any other glyph. The filled cell
count is `round(pct × 26)`; the percentage right-aligns to the grid like any other value.

**Unknown cost renders as `—`, never as `0`** — a product contract, not a display
preference. It occupies the value column exactly like a number would.

### WORKERS — 48-column grid

```
WORKERS                       3 live · 1 waiting

!  sandbox-read                     needs review
   deepseek · v4.1-flash           0m48s · $0.02
   reject cred-dir ancestors        ✓ gate green
   2 files staged                        r  open

▪  split3b                               running
   deepseek · v4.1-flash           0m52s · $0.02
   ████████████████████████    14/40 calls · 38%
   crates/p1-provider-http/
   ↳ writing route.rs

✓  split3a                         merged 4m ago
   glm · 5.3                       2m10s · $0.31
   profiles as files

·  t1-measure               queued after split3b

   ^Tab pane   a attach   x stop   ^W width
```

Rows are ordered by what needs the operator: `!` awaiting review, `▪` running, `✓` done,
`·` queued. Each worker is a three-part block — glyph + name with its state right-aligned,
then `route · profile` with elapsed and cost right-aligned, then detail lines at a single
3-space indent. Every worker carries its route and profile, because two workers on one
task are usually not the same model, and the paths it **owns**, so a collision between
two worktrees is visible before the merge rather than after.

` r ` is an inverted key, not a button.

### Promotion

| Event | Pane becomes | Duration |
|---|---|---|
| approval requested | DIFF | pinned until decided |
| worker needs review | WORKERS | pinned until decided |
| delegate spawns | WORKERS | while any worker is live |
| output over 40 lines | PEEK | 3s |
| tool fails | PEEK | 3s |
| context over 60% | PEEK | 3s |
| nothing pending | LEDGER | — |

**PEEK** is a two-line banner on BLOCK+ at the top of the ledger. The ledger underneath
does not move. Anything unresolved leaves one counted line behind (`2 live · 1 waiting`),
never a badge.

**Pinning always wins.** `^P` pins the current mode; no event may swap it. What would have
promoted itself waits as a counted line. Only two states self-pin — an approval and a
worker awaiting review — because both are the session blocked on the operator.

---

## 6. 80-column floor

Below ~100 columns the pane collapses and the transcript takes full width. `^L` forces it
back as an overlay at any width. Its contents stay reachable via `/context` and `/status`.

One dim line appears under the composer — the only bottom-of-screen state in the design,
and only because the ledger that held those three values is gone:

```
ask · claude · 12.4k        ^L ledger   ^C cancel
```

No CPU, no memory, no queue depth. Indentation, symbol set and diff hues are identical at
every width; nothing reflows into a different shape.

---

## 7. Keybindings

| Key | Action |
|---|---|
| `⏎` | send · queue steering while working · run palette entry |
| `⌥⏎` | newline · queue follow-up |
| `^C` | cancel turn · quit when idle |
| `y` `a` `p` `n` | approve once / session / project / deny |
| `r` | review a worker's staged diff |
| `^D` `^A` | next file / all files in an approval |
| `^O` | open folded output in the pane |
| `^R` | expand reasoning |
| `^Tab` | cycle pane mode |
| `^W` | cycle pane width |
| `^P` | pin pane mode |
| `^L` | force ledger overlay |
| `^T` | session timeline |
| `^G` | edit goal |
| `esc` | dismiss overlay |

---

## 8. Non-goals

Do not add these; each was excluded deliberately.

- Box-drawing borders or rounded pseudo-corners
- A persistent bottom status bar (outside the 80-col fallback)
- Braille spinners or any multi-glyph animation
- Colour as the sole carrier of state — strip every hue and all screens still read
- A third hue, or the two diff hues used for status or emphasis
- Role labels / chat bubbles / avatars
- Mouse-first affordances, button rows in place of single-key decisions
- Window previews in the pane
- Mythology names in any user-visible string (repo rule, AGENTS.md)

---

## 9. Status of this document

Section 5 (right pane) is explicitly a **concept**: it demonstrates that one pane, one
symbol set and one pair of hues stretch across a context ledger, a log viewer, a diff
reviewer and a worker dashboard without a second visual language appearing. Which modes
ship and how promotion is timed are implementation choices. Sections 1–3 and 8 are the
parts to treat as fixed — they are the vocabulary everything else is built from.
